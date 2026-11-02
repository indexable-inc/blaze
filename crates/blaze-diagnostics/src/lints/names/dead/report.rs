//! What is reported dead: of each module, its items that are not live (not under a parent
//! already reported), the dead items of its inherent impls and live traits together, the dead
//! variants of a live enum together, and the dead fields of each live variant together, each
//! group split by the level of the lint at its items; with the label of the item they are in,
//! the derives whose impls the pass ignored, how to remove a tuple struct's fields, and the enum
//! variant a dead associated item of an enum's impl is shadowed by. In an executable, first
//! what is public but used only through what is public, as `dead_code_pub_in_binary`.
//!
//! A dead item is its number and level until its group is linted; names, spans and text are
//! made only then.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`check_mod_deathness`, `lint_dead_codes`,
//! `DeadVisitor`) and `compiler/rustc_passes/src/diagnostics.rs` (`MultipleDeadCodes`,
//! `ParentInfo`, `IgnoredDerivedImpls`, `ChangeFields`, `EnumVariantSameName`,
//! `DeadCodePubInBinaryNote`) (MIT or Apache-2.0).

use blaze_ids::radix;
use hir::InFile;
use hir_def::signatures::{StructFlags, StructSignature};
use smallvec::SmallVec;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::{
    defs::{DefIdx, Id, Kind, Parent},
    level::LevelPlus,
    mark::{Live, Liveness},
    seed::Crate,
};
use stdx::list::Join;
use crate::{
    lints::registry::Level,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

/// rustc's `check_mod_deathness` of every module.
pub(super) fn report(k: &Crate<'_, '_, '_>, liveness: &Liveness) -> Vec<Found> {
    let mut found = Vec::new(); // alloc: the pass's output, once per crate
    let mut dead = Vec::new(); // alloc: setup, one buffer reused for every module
    for module in 0..k.defs.modules.len() {
        if k.executable == Some(true) {
            let pre = &liveness.pre_deferred;
            let unused_pub = |def: DefIdx| k.reach.is_reachable(def) && !pre.contains(def);
            Report { k, lint: Lint::PubInBinary, live: pre, found: &mut found, dead: &mut dead }
                .module(module, unused_pub);
        }
        Report { k, lint: Lint::DeadCode, live: &liveness.last, found: &mut found, dead: &mut dead }
            .module(module, |_| true);
    }
    found
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lint {
    DeadCode,
    PubInBinary,
}

impl Lint {
    fn name(self) -> &'static str {
        match self {
            Lint::DeadCode => "dead_code",
            Lint::PubInBinary => "dead_code_pub_in_binary",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReportOn {
    TupleField,
    NamedField,
}

#[derive(Clone, Copy)]
struct DeadItem {
    def: DefIdx,
    level: LevelPlus,
}

struct Report<'r, 'c, 'a, 'db> {
    k: &'r Crate<'c, 'a, 'db>,
    lint: Lint,
    live: &'r Live,
    found: &'r mut Vec<Found>,
    /// The dead items of one group, reused from group to group.
    dead: &'r mut Vec<DeadItem>,
}

impl Report<'_, '_, '_, '_> {
    /// rustc's `lint_dead_codes` of the free and foreign items of a module that `keep` keeps.
    fn module(&mut self, module: usize, keep: impl Fn(DefIdx) -> bool) {
        let defs = self.k.defs;
        for &item in &defs.modules[module].free {
            if !keep(item) {
                continue;
            }
            let d = &defs[item];
            if d.kind == (Kind::Impl { of_trait: false }) || (d.kind == Kind::Trait && self.live.contains(item)) {
                self.dead.clear();
                for &child in defs.children(item) {
                    if !self.is_live_code(child) {
                        let dead = self.dead_item(child);
                        self.dead.push(dead);
                    }
                }
                self.warn_multiple(item, "used", ReportOn::NamedField);
            }
            if !self.live.contains(item) {
                if !self.live.parent_live(d.parent) {
                    continue;
                }
                self.check_definition(item);
                continue;
            }
            if matches!(d.kind, Kind::Struct | Kind::Union | Kind::Enum) {
                let variants: &[DefIdx] = match d.kind {
                    Kind::Enum => defs.children(item),
                    _ => std::slice::from_ref(&item),
                };
                let mut dead_variants: SmallVec<[DeadItem; 4]> = SmallVec::new();
                for &variant in variants {
                    if !self.live.contains(variant) {
                        dead_variants.push(self.dead_item(variant));
                        continue;
                    }
                    let fields = defs.children(variant);
                    let positional = fields.first().is_some_and(|&it| defs[it].is_positional());
                    let report_on = if positional { ReportOn::TupleField } else { ReportOn::NamedField };
                    self.dead.clear();
                    for &field in fields {
                        if self.should_warn_about_field(field) {
                            let dead = self.dead_item(field);
                            self.dead.push(dead);
                        }
                    }
                    self.warn_multiple(variant, "read", report_on);
                }
                self.dead.clear();
                self.dead.extend(dead_variants);
                self.warn_multiple(item, "constructed", ReportOn::NamedField);
            }
        }
        for &item in &defs.modules[module].foreign {
            if keep(item) {
                self.check_definition(item);
            }
        }
    }

    fn dead_item(&self, def: DefIdx) -> DeadItem {
        DeadItem { def, level: self.level(def) }
    }

    /// rustc's `def_lint_level_plus`: `dead_code_pub_in_binary` is set only at the crate's root,
    /// one level for every item.
    fn level(&self, def: DefIdx) -> LevelPlus {
        match self.lint {
            Lint::DeadCode => self.k.level(def),
            Lint::PubInBinary => LevelPlus { level: Level::Allow, expect: None },
        }
    }

    fn is_live_code(&self, def: DefIdx) -> bool {
        let d = &self.k.defs[def];
        d.name.is_none() || self.live.contains(def) || d.is_underscore()
    }

    fn check_definition(&mut self, def: DefIdx) {
        if self.is_live_code(def) {
            return;
        }
        let participle = match self.k.defs[def].kind {
            Kind::AssocConst
            | Kind::AssocTy
            | Kind::AssocFn { .. }
            | Kind::Fn
            | Kind::Static
            | Kind::Const
            | Kind::TyAlias
            | Kind::Enum
            | Kind::Union
            | Kind::ForeignTy
            | Kind::Trait => "used",
            Kind::Struct => "constructed",
            _ => return,
        };
        let item = self.dead_item(def);
        self.lint_at_single_level(&[item], participle, None, ReportOn::NamedField);
    }

    /// Not a field read, of `PhantomData`, nor a positional one of a type with no size (or
    /// whose layout is not known).
    fn should_warn_about_field(&self, field: DefIdx) -> bool {
        let defs = self.k.defs;
        if self.live.contains(field) {
            return false;
        }
        let Id::Field(id) = defs[field].id else { return false };
        let db = self.k.cx.db;
        let field_hir = hir::Field::from(id);
        let ty = field_hir.ty(db);
        if let Some(hir::Adt::Struct(it)) = ty.as_adt()
            && StructSignature::of(db, it.into()).flags.contains(StructFlags::IS_PHANTOM_DATA)
        {
            return false;
        }
        if defs[field].is_positional() && field_hir.layout(db).map_or(true, |it| it.size() == 0) {
            return false;
        }
        true
    }

    /// The items in `self.dead`, not named with a leading `_`, linted in groups of one level.
    fn warn_multiple(&mut self, parent: DefIdx, participle: &str, report_on: ReportOn) {
        let defs = self.k.defs;
        let mut dead = std::mem::take(self.dead);
        dead.retain(|it| !defs[it.def].is_underscore());
        if !dead.is_empty() {
            radix::sort_by_key(&mut dead[..], |it| it.level.level as u8);
            for group in dead.chunk_by(|a, b| a.level == b.level) {
                self.lint_at_single_level(group, participle, Some(parent), report_on);
            }
        }
        dead.clear();
        *self.dead = dead;
    }

    fn lint_at_single_level(
        &mut self,
        dead_codes: &[DeadItem],
        participle: &str,
        parent: Option<DefIdx>,
        report_on: ReportOn,
    ) {
        let k = self.k;
        let cx = k.cx;
        let defs = k.defs;
        let Some(first) = dead_codes.first() else { return };
        let Some(at) = defs[first.def].node else { return };
        // alloc: the edge from here on: a lint that fires, its spans, text and notes.
        let spans: Vec<Span> = dead_codes.iter().filter_map(|it| self.ident_or_def_span(it.def)).collect();
        if spans.len() != dead_codes.len() {
            return;
        }
        let mut descr = defs[first.def].kind.descr();
        if dead_codes.iter().any(|it| defs[it.def].kind.descr() != descr) {
            descr = "associated item";
        }
        let num = dead_codes.len();
        let names = || dead_codes.iter().map(|it| name(k, it.def));
        let message = match (num > 6, num) {
            (true, _) => format!("multiple {descr}s are never {participle}"), // alloc: the edge
            (false, 1) => format!("{descr} {} is never {participle}", stdx::list::list(names(), Join::And, true)), // alloc: the edge
            (false, _) => format!("{descr}s {} are never {participle}", stdx::list::list(names(), Join::And, true)), // alloc: the edge
        };
        let mut diag = Diag::lint(self.lint.name(), message, spans[0].clone()); // clone: the edge
        for span in &spans[1..] {
            diag.primary(span.clone()); // clone: the edge
        }
        if self.lint == Lint::PubInBinary {
            diag.note(
                "in libraries, `pub` items can be used by dependent crates; in binaries, they cannot, so this `pub` item is unused",
            );
        }
        match report_on {
            ReportOn::TupleField => {
                let tuple_fields: &[DefIdx] = match parent {
                    Some(parent)
                        if defs[parent].kind == Kind::Struct
                            && defs.children(parent).first().is_some_and(|&it| defs[it].is_positional()) =>
                    {
                        defs.children(parent)
                    }
                    _ => &[],
                };
                let trailing = match tuple_fields.len() >= num {
                    true => &tuple_fields[tuple_fields.len() - num..],
                    false => &[],
                };
                let fields = if num == 1 { "field" } else { "fields" };
                if dead_codes.iter().all(|it| trailing.contains(&it.def)) {
                    let this = if num == 1 { "this" } else { "these" };
                    diag.help(format!("consider removing {this} {fields}")); // alloc: the edge
                } else {
                    diag.multipart_suggestion(
                        // alloc: the edge
                        format!(
                            "consider changing the {fields} to be of unit type to suppress this warning while preserving the field numbering, or remove the {fields}"
                        ),
                        // clone: the edge, the suggestion's spans
                        spans.iter().map(|it| (it.clone(), "()".to_owned())).collect(),
                        Applicability::HasPlaceholders,
                    );
                }
            }
            ReportOn::NamedField => {
                for item in dead_codes {
                    if let Some(variant) = self.shadowing_variant(item.def)
                        && let Some(span) = self.def_span(variant)
                    {
                        let dead_descr = defs[item.def].kind.descr();
                        diag.span_note(
                            span,
                            // alloc: the edge
                            format!(
                                "it is impossible to refer to the {dead_descr} `{}` because it is shadowed by this enum variant with the same name",
                                name(k, item.def)
                            ),
                        );
                    }
                }
            }
        }
        if let Some(parent) = parent {
            let parent_descr = defs[parent].kind.descr();
            let span = match defs[parent].kind {
                Kind::Impl { .. } => self.def_span(parent),
                _ => self.ident_span(parent),
            };
            if let Some(span) = span {
                let label = if num == 1 { descr.to_owned() } else { format!("{descr}s") }; // alloc: the edge
                diag.span_label(span, format!("{label} in this {parent_descr}")); // alloc: the edge
            }
        }
        let mut encl = parent.unwrap_or(first.def);
        if defs[encl].kind == Kind::Variant
            && let Parent::Def(adt) = defs[encl].parent
        {
            encl = adt;
        }
        if let Some(traits) = self.live.ignored.get(&encl)
            && let Some(encl_name) = &defs[encl].name
        {
            let encl_name = encl_name.as_str();
            let list = stdx::list::list(traits.iter().copied(), Join::And, true);
            let note = match traits.len() {
                // alloc: the edge
                1 => format!(
                    "`{encl_name}` has a derived impl for the trait {list}, but this is intentionally ignored during dead code analysis"
                ),
                // alloc: the edge
                _ => format!(
                    "`{encl_name}` has derived impls for the traits {list}, but these are intentionally ignored during dead code analysis"
                ),
            };
            diag.note(note);
        }
        crate::lints::code_fact(cx, &mut diag, at);
        self.found.push(Found::lint(diag, Phase::Late, at));
    }

    /// The variant of the enum an inherent impl is of with the name of `def`, an associated
    /// function or constant of the impl.
    fn shadowing_variant(&self, def: DefIdx) -> Option<DefIdx> {
        let defs = self.k.defs;
        if !matches!(defs[def].kind, Kind::AssocFn { .. } | Kind::AssocConst) {
            return None;
        }
        let Parent::Def(impl_) = defs[def].parent else { return None };
        if defs[impl_].kind != (Kind::Impl { of_trait: false }) {
            return None;
        }
        let enum_ = defs.impl_of(impl_)?.self_adt?;
        if defs[enum_].kind != Kind::Enum {
            return None;
        }
        let name = defs[def].name.as_ref()?;
        defs.children(enum_).iter().copied().find(|&it| defs[it].name.as_ref() == Some(name))
    }

    /// rustc's `def_ident_span`, or its `def_span` where a definition has no name.
    fn ident_or_def_span(&self, def: DefIdx) -> Option<Span> {
        self.ident_span(def).or_else(|| self.def_span(def))
    }

    fn ident_span(&self, def: DefIdx) -> Option<Span> {
        let cx = self.k.cx;
        let node = cx.syntax_view(self.k.defs[def].node?);
        let name = ast::AnyHasName::cast(node.value)?.name()?;
        Some(cx.span(InFile::new(node.file_id, name.syntax().text_range())))
    }

    /// rustc's `def_span`: of a variant or a field, all of it; of an item, its header.
    fn def_span(&self, def: DefIdx) -> Option<Span> {
        let cx = self.k.cx;
        let ptr = self.k.defs[def].node?;
        let node = cx.syntax_view(ptr);
        let range: TextRange = match self.k.defs[def].kind {
            Kind::Variant | Kind::Field => crate::cx::item_range_view(node.value),
            _ => crate::cx::def_range_view(node.value),
        };
        Some(cx.span(InFile::new(ptr.file_id, range)))
    }
}

/// A definition's name as rustc prints it (none: empty).
fn name<'k>(k: &'k Crate<'_, '_, '_>, def: DefIdx) -> &'k str {
    k.defs[def].name.as_ref().map_or("", |it| it.as_str())
}

