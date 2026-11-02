//! The level of each lint found, as rustc decides it, and what rustc says of it: the rule is
//! the front end's (`hir_lint_levels`: the command line, the lint attributes on the node and
//! around it, `cfg_attr`, `#![warn(warnings)]`, `forbid`, `expect`, `reason`), and this is
//! rustc's notes on where a level came from and on a future-incompatible lint; what rustc
//! says of the names in the attributes and on the command line (unknown, renamed, removed, a
//! tool's, unstable, malformed, `forbid` overruled, only at crate level), worded in
//! `builder`; the `#[expect]`s nothing fulfilled.
//!
//! - `attrs`: where rustc's walk comes to a node, and whether a span is an external macro's.
//! - `builder`: the diagnostics of what the front end's builder finds.
//!
//! Cost: a node with no attribute costs the walk a check of its kind. A lint found costs one
//! perfect hash of its name to its lint, one hash of its node (or of each node out to the
//! nearest one seen), and one bit test for a lint no attribute nor flag names; else one bit
//! test of each set of levels from there out and a scan of the 2-byte lint ids of the one that
//! has it. A level is a 12-byte `Spec`. Nothing is allocated for a lint unless it is shown.
//!
//! Follows rustc's `compiler/rustc_session/src/config.rs` (`get_cmd_lint_options`),
//! `compiler/rustc_middle/src/lint.rs` (`emit_lint_base`, `explain_lint_level_source`),
//! `compiler/rustc_lint/src/expect.rs` (`check_expectations`) and
//! `compiler/rustc_interface/src/passes.rs` (the pre-expansion lint pass before expansion, the
//! early one after resolution, the expectations checked after the late lints) (MIT or
//! Apache-2.0).

mod attrs;
mod builder;

use hir::{HirFileId, InFile};
use hir_lint_levels::{LevelCx, Levels, Source, Spec, attrs as front};
use syntax::{
    SyntaxKind, SyntaxNodePtr,
    view::{self as ast, AstNode as _, HasAttrs as _},
};

pub use crate::lints::registry::{Level, LintOptions};

use self::builder::{Said, When, owner_rank};
use crate::{
    cx::Cx,
    lints::{
        CheckCfg,
        registry::{self, Args, Lint, LintId, LintSet, Store},
        syntax::Walk,
    },
    lower::{Found, Phase},
    model::{self, Breakage, Diag, MultiSpan, Sublevel},
};

/// The lint flags rustc is given.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandLine {
    /// `-A`, `-W`, `--force-warn`, `-D`, `-F` and `--cap-lints`: what base-db holds of a
    /// crate's, and what the front end's levels read.
    pub lints: LintOptions,
    /// `--check-cfg`.
    pub check_cfg: CheckCfg,
    /// Each `--cfg` value as given, in order.
    pub cfg: Vec<String>,
    /// Whether rustc makes code (`OutputTypes::should_codegen`): an `--emit` kind other than
    /// `metadata` and `dep-info`, or no `--emit` (rustc links). `cargo check` gives
    /// `--emit=dep-info,metadata`; [`Default`] is that, not rustc's default.
    pub codegen: bool,
}

impl CommandLine {
    /// The lint flags among rustc's arguments `args`: `-A x`, `-Ax`, `--allow x`,
    /// `--allow=x` and so on, `--cap-lints` ([`LintOptions::take`]), `--check-cfg` and
    /// `--cfg`. The other arguments are not lint flags and are passed by. `--emit` is read for
    /// [`CommandLine::codegen`].
    pub fn parse(args: &[String]) -> Result<CommandLine, String> {
        let mut flags = CommandLine::default();
        // alloc: setup, the command line read once per check
        let mut emit = Vec::new();
        let mut args = Args::new(args);
        while let Some(arg) = args.arg() {
            if flags.lints.take(&mut args, arg)? {
                continue;
            }
            if let Some(spec) = args.value(arg, "--check-cfg")? {
                flags.check_cfg.add(spec)?;
                continue;
            }
            if let Some(cfg) = args.value(arg, "--cfg")? {
                // alloc: setup, as above
                flags.cfg.push(cfg.to_owned());
                continue;
            }
            if let Some(list) = args.value(arg, "--emit")? {
                for kind in list.split(',') {
                    let kind = kind.split_once('=').map_or(kind, |(kind, _)| kind);
                    if !EMIT_KINDS.contains(&kind) {
                        // alloc: an error's text
                        let expected: Vec<String> = EMIT_KINDS.iter().map(|it| format!("`{it}`")).collect();
                        // alloc: as above
                        return Err(format!("unknown emission type: `{kind}` - expected one of: {}", expected.join(", ")));
                    }
                    emit.push(kind);
                }
            }
        }
        flags.codegen = emit.is_empty() || emit.iter().any(|&it| it != "metadata" && it != "dep-info");
        Ok(flags)
    }
}

/// rustc's `--emit` kinds, in its order (`OutputType`'s shorthands).
const EMIT_KINDS: [&str; 9] = ["asm", "llvm-bc", "dep-info", "link", "llvm-ir", "metadata", "mir", "obj", "thin-link-bitcode"];

impl Said<'_, '_, '_> {
    /// `diag` of `lint` at `spec` as rustc's `emit_lint_base` emits it: nothing when it is
    /// allowed or expected (the expectation fulfilled), or in another crate's macro and not of
    /// a lint given there; else at its level, with the future-incompatible warning and link
    /// and the notes on where its level came from. A lint of the future-incompatibility
    /// report (`report_in_deps`) allowed or expected is kept for the report alone, a warning.
    fn finish(&mut self, lcx: &LevelCx<'_, '_>, lint: &'static Lint, spec: Spec, mut diag: Diag) -> Option<Diag> {
        let reported = lint.future_incompatible.is_some_and(|it| it.report_in_deps);
        if spec.level == Level::Allow && !reported {
            return None;
        }
        if diag.span.primary.iter().any(attrs::external) {
            let incompatible = lint.future_incompatible.is_some_and(|it| it.reason.edition().is_none());
            if !incompatible && !lint.report_in_external_macro {
                return None;
            }
            diag.suggestions_disabled = true;
        }
        if let Some(expect) = spec.expect {
            self.fulfill(expect);
        }
        if spec.level == Level::Expect && !reported {
            return None;
        }
        diag.level = match spec.level.is_error() {
            true => model::Level::Error,
            false => model::Level::Warning,
        };
        diag.breakage = match (reported, spec.level) {
            (false, _) => Breakage::None,
            (true, Level::Allow | Level::Expect) => Breakage::Unwritten,
            (true, _) => Breakage::Written,
        };
        if let Some(future) = lint.future_incompatible {
            if future.explain_reason {
                diag.warn(future.reason.explanation(lcx.edition));
            }
            // alloc: a note of a lint that fires
            diag.note(format!("for more information, see {}", future.reason.reference()));
        }
        self.explain(lcx, lint, spec, &mut diag);
        Some(diag)
    }

    /// rustc's `explain_lint_level_source`.
    fn explain(&self, lcx: &LevelCx<'_, '_>, lint: &'static Lint, spec: Spec, diag: &mut Diag) {
        let level = spec.level;
        if level == Level::Allow {
            return;
        }
        let once = |diag: &mut Diag, sublevel: Sublevel, message: String| {
            diag.sub(sublevel, message, MultiSpan::default(), true);
        };
        let name = lint.name;
        let shown = level.as_str();
        match lcx.source(spec.source) {
            Source::Default => {
                let note = match lcx.store.group_of(lint) {
                    // alloc: a note of a lint that fires
                    Some(group) => format!("`#[{shown}({name})]` (part of `#[{shown}({group})]`) on by default"),
                    // alloc: as above
                    None => format!("`#[{shown}({name})]` on by default"),
                };
                once(diag, Sublevel::Note, note);
            }
            Source::CommandLine { name: given, level: flagged } => {
                let flag = flagged.to_cmd_flag();
                let hyphens = name.replace('_', "-");
                if &**given == name {
                    // alloc: a note of a lint that fires
                    once(diag, Sublevel::Note, format!("requested on the command line with `{flag} {hyphens}`"));
                } else {
                    let given = given.replace('_', "-");
                    // alloc: as above
                    once(diag, Sublevel::Note, format!("`{flag} {hyphens}` implied by `{flag} {given}`"));
                    if matches!(flagged, Level::Warn | Level::Deny) {
                        let help = match lint.id() == LintId::DEAD_CODE {
                            // alloc: as above
                            true => format!("to override `{flag} {given}` add `#[expect({name})]` or `#[allow({name})]`"),
                            // alloc: as above
                            false => format!("to override `{flag} {given}` add `#[allow({name})]`"),
                        };
                        once(diag, Sublevel::Help, help);
                    }
                }
            }
            Source::Node { name: attr, at, reason } => {
                if let Some(reason) = reason {
                    diag.note(reason.as_str());
                }
                diag.sub(Sublevel::Note, "the lint level is defined here", MultiSpan::of(self.cx.span(*at)), true);
                if attr.as_str() != name {
                    // alloc: a note of a lint that fires
                    once(diag, Sublevel::Note, format!("`#[{shown}({name})]` implied by `#[{shown}({attr})]`"));
                }
            }
        }
        if let Some(warnings) = lcx.cmd_warnings
            && warnings >= Level::Deny
            && level < warnings
        {
            // alloc: a note of a lint that fires
            once(diag, Sublevel::Note, format!("the `{name}` lint ignores `-D warnings`"));
        }
    }
}

/// The nodes of `walk` the builder (`early`, or the HIR's) reads attributes of, as rustc's
/// walk of the crate comes to them. A node without costs a read of the tree's two columns,
/// kind and `Tree::attrs`; only the rest are looked at.
fn owners<'w, 'db>(walk: &'w Walk<'db>, early: bool) -> impl Iterator<Item = InFile<ast::Node<'db>>> + 'w {
    walk.trees.iter().flat_map(move |tree| {
        tree.with_attrs(move |kind| front::may_be_owner(kind, early))
            .filter(move |&node| front::owns(node, early))
            .map(move |node| InFile::new(tree.file, node))
    })
}

/// The diagnostics of `found` with the level of each lint decided, and what deciding them
/// says, each where rustc gives it: the command line's and the malformed attributes of the
/// root file as the pre-expansion lint pass runs, after the parser's errors; the rest of what
/// is said of the attributes as the early lint pass comes to them, before the early lints;
/// the `#[expect]`s nothing fulfilled after everything, unless `aborted`. When `aborted`,
/// what is of [`Phase::Late`] is dropped. What more than one of rustc's builders says is here
/// as many times, for `-Z deduplicate-diagnostics=no`: see [`builder::When`].
pub(crate) fn decide(cx: &Cx<'_, '_>, cmd: &CommandLine, found: Vec<Found>, aborted: bool) -> Vec<Diag> {
    let walk = cx.walk();
    let mut lcx = LevelCx::new(cx.db, cx.krate.base(), &cmd.lints);
    let mut said = Said::new(cx);
    let mut early = Levels::new(&mut lcx, &mut said, true, true);
    early.walk(&mut lcx, &mut said, owners(walk, true));
    let mut late = Levels::new(&mut lcx, &mut said, false, true);
    late.walk(&mut lcx, &mut said, owners(walk, false));

    let (mut pre, mut during): (Vec<_>, Vec<_>) =
        std::mem::take(&mut said.early).into_iter().partition(|it| it.when == When::Pre);
    pre.sort_by(|a, b| a.place.cmp(&b.place));
    during.sort_by(|a, b| a.place.cmp(&b.place));
    let hir = std::mem::take(&mut said.hir);
    let extra = pre.len() + during.len() + hir.len();
    let mut pre = pre.into_iter().map(|it| it.diag);
    let mut during = during.into_iter().map(|it| it.diag);
    let mut hir = hir.into_iter().map(|it| it.diag);

    let mut found = found;
    if aborted {
        found.retain(|it| it.phase != Phase::Late);
    }
    let pre_at = found.iter().position(|it| it.phase != Phase::Parse || it.diag.is_lint()).unwrap_or(found.len());
    let early_at = found
        .iter()
        .position(|it| it.phase > Phase::Resolve || (it.phase == Phase::Resolve && it.diag.is_lint()))
        .unwrap_or(found.len());
    let hir_at = found.iter().position(|it| it.phase > Phase::Resolve).unwrap_or(found.len());
    let mut out = Vec::with_capacity(found.len() + extra + late.expectations.len());
    for (index, it) in found.into_iter().enumerate() {
        if index == pre_at {
            out.extend(pre.by_ref());
        }
        if index == early_at {
            out.extend(during.by_ref());
        }
        if index == hir_at {
            out.extend(hir.by_ref());
        }
        let lint = it.diag.lint_name().map(|name| {
            registry::declared(name).unwrap_or_else(|| panic!("`{name}` is not a lint of rustc's"))
        });
        let (Some(at), Some(lint)) = (it.lint_at, lint) else {
            out.push(it.diag);
            continue;
        };
        let levels = match it.phase <= Phase::Resolve {
            true => &mut early,
            false => &mut late,
        };
        let set = match it.pre_expansion {
            // `early`'s walk has said what the attributes on the way out from `at` say
            true => levels.set_of_in_file(&mut lcx, &mut (), at),
            false => levels.set_of(&mut lcx, &mut said, at),
        };
        let spec = levels.spec(&lcx, lint, set);
        out.extend(said.finish(&lcx, lint, spec, it.diag));
    }
    out.extend(pre);
    out.extend(during);
    out.extend(hir);
    if aborted {
        return out;
    }

    let unfulfilled = LintId::UNFULFILLED_LINT_EXPECTATIONS.lint();
    // alloc: once per `#[expect]` name of the crate, ranked for rustc's order of what is
    // unfulfilled; ranked before the test, as what the loop emits can fulfill another
    let mut expectations: Vec<_> = std::mem::take(&mut late.expectations)
        .into_iter()
        .map(|it| ((owner_rank(cx, &lcx, it.node), attrs::place(&cx.vsema, it.node)), it))
        // alloc: as above
        .collect();
    expectations.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, expectation) in expectations {
        if said.is_fulfilled(expectation.id) {
            continue;
        }
        let mut diag = Diag::lint(unfulfilled.name, "this lint expectation is unfulfilled", cx.span(expectation.key));
        if let Some(reason) = expectation.reason {
            diag.note(reason.as_str());
        }
        if expectation.is_unfulfilled {
            diag.note("the `unfulfilled_lint_expectations` lint can't be expected and will always produce this message");
        }
        let set = late.set_at(&mut lcx, &mut said, expectation.node);
        let spec = late.spec(&lcx, unfulfilled, set);
        out.extend(said.finish(&lcx, unfulfilled, spec, diag));
    }
    out
}

/// rustc's HIR lint levels of a crate (`shallow_lint_levels_on`, `lint_level_at_node`), kept
/// for the whole check in [`Cx`]: the front end's builder, which says nothing, with its sets
/// kept. The command line's set is made once, and each node's set is made the first time a
/// level is asked at it or at a node in it, from the nearest node out from it already seen.
/// For a pass that must know a level before it can say what to emit (`missing_unsafe`'s
/// `unsafe_op_in_unsafe_fn`) at a node of a body; at an item, it is
/// `hir_lint_levels::lint_level`'s.
///
/// Cost: a level at a node seen before is one hash probe and the bit tests of
/// [`Levels::spec`], with nothing allocated. At a node not seen, the node is found in its tree
/// and each node between it and the nearest one seen is hashed once and read for attributes.
/// Nothing is said of the names in the attributes: [`decide`]'s builders say it.
pub(crate) struct LevelMap<'a, 'db> {
    lcx: LevelCx<'a, 'db>,
    levels: Levels<'db>,
}

impl<'a, 'db> LevelMap<'a, 'db> {
    pub(crate) fn new(cx: &Cx<'_, 'db>, opts: &'a LintOptions) -> LevelMap<'a, 'db> {
        let mut lcx = LevelCx::new(cx.db, cx.krate.base(), opts);
        let levels = Levels::new(&mut lcx, &mut (), false, false);
        LevelMap { lcx, levels }
    }

    /// The level of `lint` at the node `at`: the attributes on it and around it, the lint
    /// flags the map was made with, `warnings` and `--cap-lints`.
    pub(crate) fn level(&mut self, lint: LintId, at: InFile<SyntaxNodePtr>) -> Level {
        let set = self.levels.set_of(&mut self.lcx, &mut (), at);
        self.levels.spec(&self.lcx, lint.lint(), set).level
    }
}

/// The lints a diagnostic of may be shown in the crate: rustc's `lints_that_dont_need_to_run`,
/// the other way round. A lint is in it when its default level is not `allow`, or the command
/// line or a lint attribute anywhere in the crate (`-Z crate-attr`'s included) gives it (or a
/// group of it, or its old name) a level other than `allow`, `expect` included, as an
/// expectation needs the lint to run. Under `--cap-lints allow` it has only the lints
/// `--force-warn` names. A lint not in it is allowed at every node, so a pass that gives
/// nothing else need not run.
///
/// Cost: a pass over the lint table (a few hundred bit operations), the perfect hashes of
/// each name on the command line, and over the walk's pre-order columns one `kind` read per
/// node; for each `ATTR` node, the perfect hash of its name, and of each name in it for a level
/// attribute. Nothing is allocated but a long level attribute's list (inline up to four).
pub(crate) fn may_fire(cx: &Cx<'_, '_>, cmd: &CommandLine, walk: &Walk<'_>) -> LintSet {
    let store = Store::default();
    // The command line's level of each lint it names, as `add_command_line` sets them in
    // order: allowed, or not; a `--force-warn` or `-F` holds against what comes after it. A
    // lint behind a feature gate may be left at its default by the gate, so it is never
    // counted as allowed.
    let mut allowed = LintSet::EMPTY;
    let mut set = LintSet::EMPTY;
    let mut held = LintSet::EMPTY;
    let mut forced = LintSet::EMPTY;
    for (name, level) in &cmd.lints.opts {
        let (tool, name) = match name.split_once("::") {
            Some((tool, name)) => (Some(tool), name),
            None => (None, name.as_str()),
        };
        for lint in store.lints_named(tool, name) {
            let id = lint.id();
            if held.contains(id) {
                continue;
            }
            match *level {
                Level::Allow => {
                    set.remove(id);
                    if lint.feature_gate.is_none() {
                        allowed.insert(id);
                    }
                }
                _ => {
                    allowed.remove(id);
                    set.insert(id);
                }
            }
            if matches!(*level, Level::ForceWarn | Level::Forbid) {
                held.insert(id);
            }
            if *level == Level::ForceWarn {
                forced.insert(id);
            }
        }
    }
    if cmd.lints.cap == Some(Level::Allow) {
        return forced;
    }
    for lint in registry::all() {
        if lint.default_level(cx.edition) != Level::Allow && !allowed.contains(lint.id()) {
            set.insert(lint.id());
        }
    }
    for tree in &walk.trees {
        for node in tree.of_kind(|it| it == SyntaxKind::ATTR) {
            let Some(attr) = ast::Attr::cast(node) else { continue };
            front::not_allowed(attr, &store, walk.cfg, cx.edition, &mut set);
        }
    }
    for &file in base_db::applied_crate_attr_files(cx.db, cx.krate.base()).iter() {
        let Some(root) = ast::SourceFile::cast(cx.root(HirFileId::from(file))) else { continue };
        for attr in root.attrs() {
            front::not_allowed(attr, &store, walk.cfg, cx.edition, &mut set);
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use syntax::view::{AstNode as _, HasName as _};

    use super::*;

    /// `check` given the context of the first workspace crate of `fixture`, checked with the
    /// lint flags `args`.
    fn with_cx(fixture: &str, args: &[&str], check: impl FnOnce(&Cx<'_, '_>)) {
        let checked = crate::tests::checked(fixture);
        let args: Vec<String> = args.iter().map(|it| (*it).to_owned()).collect();
        let cmd = CommandLine::parse(&args).unwrap();
        let db = &checked.db;
        hir::attach_db(db, || {
            let krate = hir::check::workspace_crates(db)[0];
            let frame = crate::tests::frame();
            let cx = Cx::new(db, krate, checked.files(), &frame).with_lints(&cmd);
            check(&cx);
        });
    }

    /// Each function the walk comes to, by name, with where it is.
    fn fns(cx: &Cx<'_, '_>) -> Vec<(String, InFile<SyntaxNodePtr>)> {
        let mut out = Vec::new();
        for tree in &cx.walk().trees {
            for &node in &tree.nodes {
                let Some(name) = ast::Fn::cast(node).and_then(|it| it.name()) else { continue };
                out.push((name.text().to_owned(), tree.ptr(node)));
            }
        }
        out
    }

    /// [`Cx::lint_level`] of `lint` at each function of the first workspace crate of
    /// `fixture`, by name, with the lint flags `args`. Each is also asked of a map made for it
    /// alone, which has seen no other node: what the check's map kept gives the same level.
    fn levels_of_fns(fixture: &str, args: &[&str], lint: &str) -> Vec<(String, Level)> {
        let lint = registry::declared(lint).unwrap().id();
        let mut out = Vec::new();
        with_cx(fixture, args, |cx| {
            for (name, at) in fns(cx) {
                let level = cx.lint_level(lint, at);
                let alone = LevelMap::new(cx, &cx.lint_flags().lints).level(lint, at);
                assert_eq!(level, alone, "the level at `{name}`");
                assert_eq!(cx.lint_level(lint, at), level, "the level at `{name}`, asked again");
                out.push((name, level));
            }
        });
        out
    }

    const LEVELS_FIXTURE: &str = r#"
//- /main.rs crate:main edition:2021
#![warn(missing_docs)]
mod m {
    #[deny(unreachable_pub)]
    pub fn denied() {}
    #[expect(single_use_lifetimes)]
    pub fn expected() {}
    #[allow(unused_qualifications)]
    pub fn allowed() {}
    #[cfg_attr(all(), warn(clippy::all, elided_lifetimes_in_paths))]
    pub fn by_cfg_attr() {}
}
fn main() {}
"#;

    #[test]
    fn a_lint_may_fire_where_an_attribute_or_the_command_line_does_not_allow_it() {
        let ids = |names: &[&str]| -> Vec<LintId> { names.iter().map(|it| registry::declared(it).unwrap().id()).collect() };
        with_cx(LEVELS_FIXTURE, &["-W", "unused-results", "-A", "dead-code"], |cx| {
            for id in ids(&[
                "missing_docs",
                "unreachable_pub",
                "single_use_lifetimes",
                "elided_lifetimes_in_paths",
                "unused_results",
                "unused_variables",
            ]) {
                assert!(cx.lint_may_fire(id), "{} may fire", id.lint().name);
            }
            for id in ids(&["unused_qualifications", "unsafe_code", "variant_size_differences", "dead_code"]) {
                assert!(!cx.lint_may_fire(id), "{} is allowed everywhere", id.lint().name);
            }
        });
        with_cx(LEVELS_FIXTURE, &["--cap-lints", "allow", "--force-warn", "unsafe-code"], |cx| {
            assert!(cx.lint_may_fire(LintId::named("unsafe_code")));
            for id in ids(&["missing_docs", "unreachable_pub", "unused_variables"]) {
                assert!(!cx.lint_may_fire(id), "{} is capped to allow", id.lint().name);
            }
        });
    }

    /// Every lint a level of the check's map is not `allow` for may fire: the set holds all
    /// the map can give.
    #[test]
    fn what_may_fire_holds_every_level_given() {
        for args in [&[][..], &["-D", "warnings"], &["-W", "rust-2018-idioms"], &["--cap-lints", "warn"]] {
            with_cx(LEVELS_FIXTURE, args, |cx| {
                for (name, at) in fns(cx) {
                    for lint in registry::all() {
                        if cx.lint_level(lint.id(), at) != Level::Allow {
                            assert!(cx.lint_may_fire(lint.id()), "{} at `{name}` with {args:?}", lint.name);
                        }
                    }
                }
            });
        }
    }

    /// A level asked again at a node, and whether a lint may fire, allocate nothing.
    #[test]
    fn a_level_seen_and_a_lint_that_may_fire_are_reads() {
        with_cx(LEVELS_FIXTURE, &[], |cx| {
            let fns = fns(cx);
            let lint = LintId::named("unreachable_pub");
            for (_, at) in &fns {
                cx.lint_level(lint, *at);
            }
            cx.lint_may_fire(lint);
            cx.walk();
            blaze_alloc_count::assert_at_most(0, "a level at a node seen", || {
                for (_, at) in &fns {
                    for other in registry::lints() {
                        cx.lint_level(other.id(), *at);
                    }
                }
            });
            blaze_alloc_count::assert_at_most(0, "whether a lint may fire, and the walk again", || {
                for other in registry::all() {
                    cx.lint_may_fire(other.id());
                }
                cx.walk();
            });
        });
    }

    /// A crate of one file with `n` functions, each with two lint attributes, in modules of ten
    /// with one of their own, under a crate-level one.
    fn attributed_fns(n: usize) -> String {
        let mut out = String::from("//- /main.rs crate:main edition:2021\n#![warn(missing_docs)]\n");
        for m in 0..n.div_ceil(10) {
            out.push_str(&format!("#[deny(unreachable_pub)]\nmod m{m} {{\n"));
            for i in m * 10..n.min(m * 10 + 10) {
                out.push_str(&format!(
                    "    #[allow(dead_code)]\n    #[warn(elided_lifetimes_in_paths, unused_results)]\n    pub fn f{i}() {{}}\n"
                ));
            }
            out.push_str("}\n");
        }
        out.push_str("fn main() {}\n");
        out
    }

    /// The allocations of a level map made for the check of [`attributed_fns`]`(n)` with lints
    /// on, and of the first level asked at each of its functions, when every node from one out
    /// to the nearest seen is read for attributes.
    fn level_map_build_allocations(n: usize) -> u64 {
        let lint = LintId::named("unreachable_pub");
        let mut used = 0;
        with_cx(&attributed_fns(n), &["-W", "unused-results", "-D", "warnings"], |cx| {
            let fns = fns(cx);
            assert_eq!(fns.len(), n + 1, "the walk comes to every function");
            used = blaze_alloc_count::measure(|| {
                let mut map = LevelMap::new(cx, &cx.lint_flags().lints);
                for (_, at) in &fns {
                    map.level(lint, *at);
                }
            })
            .1
            .allocations;
        });
        used
    }

    /// The level map's build reads each lint attribute in place: what grows with the crate is
    /// its tables, which double, never an allocation per node or attribute.
    #[test]
    fn building_the_level_map_allocates_per_table_not_per_attribute() {
        let (small, large) = (level_map_build_allocations(4), level_map_build_allocations(400));
        // Under ten tables (the sets' columns, the node table, the kept sources), each doubling
        // at most eight times from 4 functions to 400: one per node would be 396 more.
        assert!(large <= small + 80, "the level map's build: {small} allocations at 4 functions, {large} at 400");
    }

    #[test]
    fn level_at_a_node_has_its_attributes_the_command_line_and_the_cap() {
        let fixture = r#"
//- /main.rs crate:main edition:2021
#![deny(unsafe_op_in_unsafe_fn)]
mod m {
    #[allow(unsafe_op_in_unsafe_fn)]
    pub fn allowed() {}
    pub fn denied() {}
}
#[expect(unsafe_op_in_unsafe_fn)]
fn expected() {}
fn main() {}
"#;
        let lint = "unsafe_op_in_unsafe_fn";
        let named = |levels: &[(&str, Level)]| -> Vec<(String, Level)> {
            levels.iter().map(|&(name, level)| (name.to_owned(), level)).collect()
        };
        assert_eq!(
            levels_of_fns(fixture, &[], lint),
            named(&[("allowed", Level::Allow), ("denied", Level::Deny), ("expected", Level::Expect), ("main", Level::Deny)])
        );
        assert_eq!(
            levels_of_fns(fixture, &["--cap-lints", "warn"], lint),
            named(&[("allowed", Level::Allow), ("denied", Level::Warn), ("expected", Level::Expect), ("main", Level::Warn)])
        );
        let plain = "//- /main.rs crate:main edition:2021\nfn main() {}\n";
        assert_eq!(levels_of_fns(plain, &[], lint), named(&[("main", Level::Allow)]));
        assert_eq!(levels_of_fns(plain, &["-F", "unsafe-op-in-unsafe-fn"], lint), named(&[("main", Level::Forbid)]));
    }

    /// The check's levels and the front end's query are one rule: at every function of the level
    /// fixtures, [`LevelMap`] and `hir_lint_levels::lint_levels_on` agree on every lint. No
    /// lint flags are given, because the query reads them from the crate's base-db input and
    /// the map from the command line.
    #[test]
    fn the_checks_levels_are_the_front_ends() {
        for (fixture, functions) in [(LEVELS_FIXTURE.to_owned(), 5), (attributed_fns(12), 13)] {
            with_cx(&fixture, &[], |cx| {
                let opts = LintOptions::default();
                let mut map = LevelMap::new(cx, &opts);
                let def_map = hir_def::nameres::crate_def_map(cx.db, cx.krate.base());
                let mut asked = 0;
                for (_, module) in def_map.modules(cx.db) {
                    for def in module.scope(cx.db).declarations() {
                        let hir_def::ModuleDefId::FunctionId(id) = def else { continue };
                        let owner = hir_def::ExpressionStoreOwnerId::Body(hir_def::DefWithBodyId::FunctionId(id));
                        let node = hir_lint_levels::query::owner_node(cx.db, owner);
                        let at = InFile::new(node.file_id, ast::ptr(node.value));
                        let front = hir_lint_levels::lint_levels_on(cx.db, owner, ());
                        for lint in registry::all() {
                            assert_eq!(map.level(lint.id(), at), front.level(lint.id()), "{} at {at:?}", lint.name);
                        }
                        asked += 1;
                    }
                }
                assert_eq!(asked, functions, "every function of the fixture");
            });
        }
    }

    #[test]
    fn emit_says_whether_rustc_makes_code() {
        let codegen = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|&it| it.to_owned()).collect();
            CommandLine::parse(&args).map(|it| it.codegen)
        };
        assert_eq!(codegen(&[]), Ok(true));
        assert_eq!(codegen(&["--emit=dep-info,metadata"]), Ok(false));
        assert_eq!(codegen(&["--emit", "metadata=out/lib.rmeta"]), Ok(false));
        assert_eq!(codegen(&["--emit=metadata", "--emit=link"]), Ok(true));
        assert_eq!(codegen(&["--emit=mir"]), Ok(true));
        assert_eq!(
            codegen(&["--emit=rlib"]),
            Err("unknown emission type: `rlib` - expected one of: `asm`, `llvm-bc`, `dep-info`, `link`, \
                 `llvm-ir`, `metadata`, `mir`, `obj`, `thin-link-bitcode`"
                .to_owned())
        );
        assert!(!CommandLine::default().codegen);
    }
}
