//! The printer: rustc's `FmtPrinter` and `AbsolutePathPrinter`, its regions, binders and
//! generic arguments, and the paths of definitions (`print_def_path`: trimmed, visible through
//! re-exports, or the definition's own path with its impl, trait and closures), spelled for a
//! diagnostic, for `type_name`, for a symbol or for debug info. Ported from
//! `compiler/rustc_middle/src/ty/print/pretty.rs`, `compiler/rustc_middle/src/ty/print/mod.rs`,
//! `compiler/rustc_const_eval/src/util/type_name.rs`,
//! `compiler/rustc_symbol_mangling/src/v0.rs`,
//! `compiler/rustc_codegen_ssa/src/debuginfo/type_names.rs` and `own_args_no_defaults` of
//! `compiler/rustc_middle/src/ty/generics.rs` (MIT or Apache-2.0).

use std::alloc::Global;
use std::fmt::Write as _;

use base_db::Db;
use blaze_hash::HashSet;
use blaze_mem::scratch::Lent;
use hir_def::{
    AdtId, ExpressionStoreOwnerId, ExternCrateId, FunctionId, GenericDefId, ItemContainerId,
    Lookup,
    hir::{ClosureKind, CoroutineKind, CoroutineSource, ExprId, generics::GenericParams},
};
use hir_expand::name::Name;
use hir_ty::db::HirDatabase as _;
use hir_ty::next_solver::{
    Binder, BoundRegion, DbInterner, EarlyParamRegion, GenericArg, GenericArgKind, GenericArgs,
    Region, RegionKind, TraitClause, TraitRef, Ty, TyKind,
};
use intern::Symbol;
use rustc_type_ir::{
    BoundRegionKind, BoundVarIndexKind, ClausePolarity, DebruijnIndex, INNERMOST,
    TypeSuperVisitable, TypeVisitable, TypeVisitor,
    inherent::{BoundExistentialPredicates as _, GenericArgs as _, IntoKind as _},
};
use smallvec::SmallVec;
use span::Edition;

use crate::def::{self, Def, Parent};

/// Which of rustc's printers a name is printed by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Spelling {
    /// rustc's `FmtPrinter` in a diagnostic: trimmed and visible paths, regions as rustc shows
    /// them, closures by their span. Needs a [`DiagnosticHost`].
    Diagnostic,
    /// `core::any::type_name`: full paths, every lifetime `'_`, every closure `{{closure}}`.
    TypeName,
    /// A demangled v0 symbol: no lifetimes, `<T as Trait>::f`, `f::<A>`, closures numbered.
    Symbol,
    /// The name of a type or function in debug info: no lifetimes, defaults written out,
    /// `{impl#N}`, closures numbered and called after what they are.
    Debug,
}

/// rustc's printing flags: the spelling, `with_forced_trimmed_paths!`,
/// `with_no_trimmed_paths!`, regions erased and anonymized first
/// (`erase_and_anonymize_regions`), and the type length limit of `FmtPrinter::new_with_limit`.
/// Only the diagnostic spelling reads the flags.
#[derive(Clone, Copy, Debug)]
pub struct Mode {
    pub spelling: Spelling,
    pub forced: bool,
    pub untrimmed: bool,
    pub erase: bool,
    pub type_limit: Option<usize>,
}

impl Mode {
    pub const NORMAL: Mode = Mode {
        spelling: Spelling::Diagnostic,
        forced: false,
        untrimmed: false,
        erase: false,
        type_limit: None,
    };
    pub const UNTRIMMED: Mode = Mode { untrimmed: true, ..Mode::NORMAL };
    pub const ERASED: Mode = Mode { erase: true, ..Mode::NORMAL };
    pub const TYPE_NAME: Mode = Mode { spelling: Spelling::TypeName, ..Mode::NORMAL };
    pub const SYMBOL: Mode = Mode { spelling: Spelling::Symbol, ..Mode::NORMAL };
    pub const DEBUG: Mode = Mode { spelling: Spelling::Debug, ..Mode::NORMAL };

    pub fn forced(self) -> Mode {
        Mode { forced: true, ..self }
    }

    pub fn limited(self, limit: usize) -> Mode {
        Mode { type_limit: Some(limit), ..self }
    }
}

/// What a diagnostic's printing reads of the crate being checked: its tables of trimmed and
/// visible paths, and the source a closure's span and an anonymous constant are printed from.
pub trait DiagnosticHost {
    /// The crate the diagnostic is about: its own paths start without the crate's name.
    fn krate(&self) -> base_db::Crate;
    fn edition(&self) -> Edition;
    /// The one name `def` is known by across the crate and its direct dependencies.
    fn trimmed(&self, def: Def) -> Option<Name>;
    /// rustc's `visible_parent_map`.
    fn visible_parent(&self, def: Def) -> Option<Def>;
    /// What a re-export in `parent` calls `def`, other than `_`.
    fn reexport_name(&self, parent: Def, def: Def) -> Option<Name>;
    /// The `extern crate` item the crate names `krate` by, when it writes one.
    fn extern_crate(&self, krate: base_db::Crate) -> Option<ExternCrateId>;
    /// `file:l:c: l:c` of rustc's `def_span` of the closure-like expression `expr` of `owner`
    /// (`file:l:c` of the start, the file's name only, when `short`), written to `out`.
    fn closure_span(
        &self,
        owner: ExpressionStoreOwnerId,
        expr: ExprId,
        kind: ClosureKind,
        short: bool,
        out: &mut String,
    );
    /// The source text of the anonymous constant `expr` of `owner`, written to `out`.
    fn anon_const_text(&self, owner: ExpressionStoreOwnerId, expr: ExprId, out: &mut String);
}

/// rustc's `WrapBinderMode`, with a function item's signature told apart: its named bound
/// regions are the function's late-bound lifetimes, the names rustc prints for them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BinderKind {
    ForAll,
    Signature,
    Unsafe,
}

/// The names given to the regions one binder binds, by bound variable.
struct Frame {
    names: SmallVec<[(u32, Symbol); 4]>,
}

/// rustc's names for anonymous regions, `'a` to `'z`.
const LETTERS: [&str; 26] = [
    "'a", "'b", "'c", "'d", "'e", "'f", "'g", "'h", "'i", "'j", "'k", "'l", "'m", "'n", "'o", "'p",
    "'q", "'r", "'s", "'t", "'u", "'v", "'w", "'x", "'y", "'z",
];

/// The rule not reproduced when no more specific one is named.
pub(crate) const NOT_LOOKED_UP: &str =
    "a part of it whose path, constant or generic argument could not be looked up";

pub struct Printer<'h, 't> {
    pub db: &'t Db,
    pub interner: DbInterner<'t>,
    pub(crate) host: Option<&'h dyn DiagnosticHost>,
    pub mode: Mode,
    pub in_value: bool,
    pub(crate) out: Lent<String>,
    empty_path: bool,
    no_visible: bool,
    printed_type_count: usize,
    frames: Vec<Frame>,
    used_names: HashSet<Symbol, Global>,
    region_index: usize,
    /// The first rule of rustc's found not reproduced, in a spelling other than a
    /// diagnostic's: what is printed after it is not the name.
    pub(crate) refused: Option<&'static str>,
    /// Whether a type parameter is printed `_`: `type_name` and debug info print the impl of
    /// what a body's item is written in with its own parameters, each `_`.
    pub(crate) params_as_holes: bool,
}

/// What rustc's `generics_of` says of a definition, as path printing reads it.
pub struct Generics {
    pub id: Option<GenericDefId>,
    /// The trait or impl an associated item is in.
    pub parent: Option<Def>,
    pub parent_count: usize,
    pub count: usize,
    pub has_self: bool,
}

impl<'h, 't> Printer<'h, 't> {
    /// A printer for a diagnostic about `host`'s crate.
    pub fn diagnostic(
        db: &'t Db,
        host: &'h dyn DiagnosticHost,
        mode: Mode,
        in_value: bool,
    ) -> Self {
        debug_assert_eq!(mode.spelling, Spelling::Diagnostic);
        Self::with(db, Some(host), mode, in_value)
    }

    /// A printer of `type_name`'s, a symbol's or debug info's spelling.
    pub fn spelled(db: &'t Db, spelling: Spelling) -> Self {
        assert_ne!(
            spelling,
            Spelling::Diagnostic,
            "a diagnostic is printed for a crate, by `Printer::diagnostic`"
        );
        Self::with(db, None, Mode { spelling, ..Mode::NORMAL }, false)
    }

    fn with(db: &'t Db, host: Option<&'h dyn DiagnosticHost>, mode: Mode, in_value: bool) -> Self {
        Printer {
            db,
            interner: match host {
                Some(host) => DbInterner::new_with(db, host.krate()),
                None => DbInterner::new_no_crate(db),
            },
            host,
            mode,
            in_value,
            out: blaze_mem::scratch::lend_string(),
            empty_path: false,
            no_visible: false,
            printed_type_count: 0,
            frames: Vec::new(), // alloc: empty, allocates nothing
            used_names: HashSet::new_in(Global),
            region_index: 0,
            refused: None,
            params_as_holes: false,
        }
    }

    pub(crate) fn host(&self) -> &'h dyn DiagnosticHost {
        self.host.expect("a diagnostic's printer has its host")
    }

    pub fn spelling(&self) -> Spelling {
        self.mode.spelling
    }

    fn diagnostic_spelling(&self) -> bool {
        self.mode.spelling == Spelling::Diagnostic
    }

    /// The text printed, allocated once at its length.
    pub fn finish(self) -> String {
        // alloc: the text printed is the result
        String::from(self.out.as_str())
    }

    /// [`Printer::finish`], or the rule of rustc's not reproduced for the part that stopped it.
    pub fn finish_spelled(self) -> Result<String, &'static str> {
        match self.refused {
            Some(why) => Err(why),
            None => Ok(self.finish()),
        }
    }

    /// Stops printing a spelled name: `why` names the rule of rustc's that is not reproduced.
    pub(crate) fn refuse(&mut self, why: &'static str) {
        debug_assert!(!self.diagnostic_spelling(), "a diagnostic prints every type");
        self.refused.get_or_insert(why);
    }

    pub fn write_str(&mut self, s: &str) {
        self.out.push_str(s);
    }

    pub fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) {
        // Writing into a `String` does not fail.
        let _ = self.out.write_fmt(args);
    }

    /// An identifier: in a diagnostic `r#` before one that is a keyword in the crate's edition.
    pub fn write_name(&mut self, name: &Name) {
        match self.host {
            Some(host) => {
                let edition = host.edition();
                let _ = write!(self.out, "{}", name.display_no_db(edition));
            }
            None => self.out.push_str(name.as_str()),
        }
    }

    /// rustc's `should_truncate`: more types printed than the length limit allows.
    pub(crate) fn should_truncate(&self) -> bool {
        self.mode.type_limit.is_some_and(|limit| self.printed_type_count > limit)
    }

    pub(crate) fn count_type(&mut self) {
        self.printed_type_count += 1;
    }

    /// Runs `f` with paths forced trimmed, as `with_forced_trimmed_paths!` does.
    pub(crate) fn forced_trimmed(&mut self, f: impl FnOnce(&mut Self)) {
        let was = std::mem::replace(&mut self.mode.forced, true);
        f(self);
        self.mode.forced = was;
    }

    // Regions.

    /// rustc's `Region::get_name`, of the regions as this printer sees them.
    fn region_name(&self, r: Region<'t>) -> Option<Symbol> {
        let kind = r.kind();
        if self.mode.erase && !matches!(kind, RegionKind::ReBound(..)) {
            return None;
        }
        match kind {
            RegionKind::ReEarlyParam(p) => early_name(self.db, p).filter(|it| it.as_str() != "'_"),
            RegionKind::ReLateParam(p) => late_name(self.db, p.kind),
            RegionKind::ReBound(BoundVarIndexKind::Bound(d), br) => self.bound_name(d, br),
            RegionKind::RePlaceholder(p) => late_name(self.db, p.bound),
            _ => None,
        }
    }

    fn bound_name(&self, d: DebruijnIndex, br: BoundRegion<'t>) -> Option<Symbol> {
        let frame = self.frames.len().checked_sub(1 + d.as_usize())?;
        let var = br.var.as_u32();
        self.frames[frame].names.iter().find(|(v, _)| *v == var).map(|&(_, name)| name)
    }

    /// rustc's `should_print_optional_region`: whether `&'a T` and `dyn Trait + 'a` show it.
    /// `type_name` shows a bound one as `'_`; a symbol and debug info show none.
    pub(crate) fn should_print_optional_region(&self, r: Region<'t>) -> bool {
        match self.mode.spelling {
            Spelling::TypeName => return matches!(r.kind(), RegionKind::ReBound(..)),
            Spelling::Symbol | Spelling::Debug => return false,
            Spelling::Diagnostic => {}
        }
        if self.mode.forced {
            return false;
        }
        match r.kind() {
            RegionKind::ReStatic => !self.mode.erase,
            _ => self.region_name(r).is_some(),
        }
    }

    /// rustc's `pretty_print_region`; `'_` for every region outside a diagnostic.
    pub(crate) fn print_region(&mut self, r: Region<'t>) {
        let kind = r.kind();
        if !self.diagnostic_spelling()
            || (self.mode.erase && !matches!(kind, RegionKind::ReBound(..)))
        {
            return self.write_str("'_");
        }
        match kind {
            RegionKind::ReEarlyParam(p) => match early_name(self.db, p) {
                Some(name) => self.write_str(name.as_str()),
                None => self.write_str("'_"),
            },
            RegionKind::ReStatic => self.write_str("'static"),
            _ => match self.region_name(r) {
                Some(name) => self.write_str(name.as_str()),
                None => self.write_str("'_"),
            },
        }
    }

    /// rustc's `wrap_binder` and `name_all_regions`: names the regions `binder` binds (keeping
    /// a name it has, else the next of `'a`, `'b`, … no region in sight is called), prints
    /// `for<..> ` unless paths are forced trimmed, then `f` of the bound value. Outside a
    /// diagnostic no region is named and no binder printed.
    pub(crate) fn wrap_binder<T>(
        &mut self,
        binder: &Binder<'t, T>,
        kind: BinderKind,
        f: impl FnOnce(&mut Self, &T),
    ) where
        T: TypeVisitable<DbInterner<'t>>,
    {
        let value = binder.as_ref().skip_binder();
        if !self.diagnostic_spelling() {
            return f(self, value);
        }
        if self.frames.is_empty() {
            let mut used = HashSet::new_in(Global);
            if !self.mode.erase {
                value.visit_with(&mut UsedNames {
                    db: self.db,
                    depth: INNERMOST,
                    signature: kind == BinderKind::Signature,
                    out: &mut used,
                });
            }
            self.used_names = used;
            self.region_index = 0;
        }
        let mut bound = BoundAt { depth: INNERMOST, found: SmallVec::new() };
        value.visit_with(&mut bound);

        let letters: SmallVec<[Symbol; 26]> = match bound.found.is_empty() {
            true => SmallVec::new(),
            // alloc: none, at most 26 inline in the `SmallVec`
            false => LETTERS
                .iter()
                .map(|it| Symbol::intern(it))
                .filter(|it| !self.used_names.contains(it))
                .collect(),
        };
        let mut taken = 0;
        let mut region_index = self.region_index;
        let mut names: SmallVec<[(u32, Symbol); 4]> = SmallVec::new();
        for br in bound.found {
            let own = match br.kind {
                BoundRegionKind::Named(_) if kind == BinderKind::Signature && !self.mode.erase => {
                    late_name(self.db, br)
                }
                BoundRegionKind::NamedForPrinting(name)
                    if !self.mode.erase && name.0.as_str() != "'_" =>
                {
                    Some(name.0)
                }
                _ => None,
            };
            let name = own.unwrap_or_else(|| {
                loop {
                    let name = match letters.get(taken) {
                        Some(it) => {
                            taken += 1;
                            *it
                        }
                        // alloc: a region past `'z` named, printing a type, the edge
                        None => Symbol::intern(&format!("'z{}", region_index - letters.len())),
                    };
                    region_index += 1;
                    if !self.used_names.contains(&name) {
                        break name;
                    }
                }
            });
            names.push((br.var.as_u32(), name));
        }

        let start = if kind == BinderKind::Unsafe { "unsafe<" } else { "for<" };
        if !self.mode.forced || kind == BinderKind::Unsafe {
            for (i, (_, name)) in names.iter().enumerate() {
                self.out.push_str(if i == 0 { start } else { ", " });
                self.out.push_str(name.as_str());
            }
        }
        let printed = !names.is_empty() && (!self.mode.forced || kind == BinderKind::Unsafe);
        if kind == BinderKind::Unsafe && names.is_empty() {
            self.out.push_str(start);
        }
        if printed || kind == BinderKind::Unsafe {
            self.out.push_str("> ");
        }

        let old_index = self.region_index;
        self.frames.push(Frame { names });
        self.region_index = region_index;
        f(self, value);
        self.frames.pop();
        self.region_index = old_index;
    }

    // Generic arguments.

    /// Whether `arg` is printed among generic arguments: a symbol and debug info have no
    /// lifetimes.
    fn prints_arg(&self, arg: GenericArg<'t>) -> bool {
        !matches!(self.mode.spelling, Spelling::Symbol | Spelling::Debug) || arg.region().is_none()
    }

    pub(crate) fn print_arg(&mut self, arg: GenericArg<'t>) {
        match arg.kind() {
            GenericArgKind::Lifetime(r) => self.print_region(r),
            GenericArgKind::Type(ty) => self.print_type(ty),
            GenericArgKind::Const(c) => self.print_const(c),
        }
    }

    pub fn comma_sep_args(&mut self, args: &[GenericArg<'t>]) {
        let mut first = true;
        for arg in args {
            if !self.prints_arg(*arg) {
                continue;
            }
            if !std::mem::replace(&mut first, false) {
                self.write_str(", ");
            }
            self.print_arg(*arg);
        }
    }

    pub(crate) fn comma_sep_types(&mut self, tys: &[Ty<'t>]) {
        for (i, ty) in tys.iter().enumerate() {
            if i > 0 {
                self.write_str(", ");
            }
            self.print_type(*ty);
        }
    }

    /// rustc's `generic_delimiters`: `<..>`, out of value namespace inside.
    pub(crate) fn generic_delimiters(&mut self, f: impl FnOnce(&mut Self)) {
        self.write_str("<");
        let was = std::mem::replace(&mut self.in_value, false);
        f(self);
        self.in_value = was;
        self.write_str(">");
    }

    /// The arguments part of rustc's `print_path_with_generic_args`: `::<..>` in value
    /// namespace (never in `type_name` and debug info), `<..>` else, nothing for none.
    pub(crate) fn path_generic_args(&mut self, args: &[GenericArg<'t>]) {
        if !args.iter().any(|it| self.prints_arg(*it)) {
            return;
        }
        if self.in_value && matches!(self.mode.spelling, Spelling::Diagnostic | Spelling::Symbol) {
            self.write_str("::");
        }
        self.generic_delimiters(|p| p.comma_sep_args(args));
    }

    /// rustc's `print_path_with_simple` of a named segment.
    fn path_segment(&mut self, name: &Name) {
        if !self.empty_path {
            self.write_str("::");
        }
        self.write_name(name);
        self.empty_path = false;
    }

    /// A segment of the path that is not a name: `{closure#0}`, `{impl#1}`.
    fn path_segment_fmt(&mut self, args: std::fmt::Arguments<'_>) {
        if !self.empty_path {
            self.write_str("::");
        }
        self.write_fmt(args);
        self.empty_path = false;
    }

    /// The closures something is written in, outermost first, by their places among the
    /// closures of what they are written in: `{closure#N}`, `{{closure}}` to `type_name`, and
    /// to a debugger what they are, `{async_block#N}`.
    pub(crate) fn closure_segments(&mut self, closures: &[(usize, ClosureKind)]) {
        for &(place, kind) in closures {
            match self.mode.spelling {
                Spelling::Diagnostic | Spelling::Symbol => {
                    self.path_segment_fmt(format_args!("{{closure#{place}}}"))
                }
                Spelling::TypeName => self.path_segment_fmt(format_args!("{{{{closure}}}}")),
                Spelling::Debug => {
                    self.path_segment_fmt(format_args!("{{{}#{place}}}", kind_label(kind)))
                }
            }
        }
    }

    // Paths.

    /// rustc's `pretty_print_value_path`.
    pub fn print_value_path(&mut self, def: Def, args: &[GenericArg<'t>]) {
        let was = std::mem::replace(&mut self.in_value, true);
        self.print_def_path(def, args);
        self.in_value = was;
    }

    /// rustc's `FmtPrinter::print_def_path`; outside a diagnostic, rustc's
    /// `default_print_def_path`.
    pub fn print_def_path(&mut self, def: Def, args: &[GenericArg<'t>]) {
        if self.refused.is_some() {
            return;
        }
        if self.diagnostic_spelling() && args.is_empty() {
            if self.try_print_trimmed(def) {
                return;
            }
            if !self.no_visible {
                let at = self.out.len();
                let was_empty = self.empty_path;
                if self.try_print_visible(def, &mut SmallVec::new()) {
                    return;
                }
                self.out.truncate(at);
                self.empty_path = was_empty;
            }
        }
        self.default_print_def_path(def, args);
    }

    /// rustc's `try_print_trimmed_def_path`.
    fn try_print_trimmed(&mut self, def: Def) -> bool {
        if self.mode.forced && self.force_print_trimmed(def) {
            return true;
        }
        if self.mode.untrimmed {
            return false;
        }
        let Some(name) = self.host().trimmed(def) else { return false };
        self.write_name(&name);
        true
    }

    /// rustc's `force_print_trimmed_def_path`: a type, trait or free item by its name, an
    /// associated item of a trait as `Trait::item`, a variant as `Enum::Variant`, or by its
    /// unique name.
    fn force_print_trimmed(&mut self, def: Def) -> bool {
        let db = self.db;
        if let Def::Variant(_) = def
            && let Some(name) = self.host().trimmed(def)
        {
            let name = self.local_name(name, def);
            self.write_name(&name);
            return true;
        }
        let Some(name) = def.name(db) else { return false };
        match def {
            Def::Function(_) | Def::Const(_) | Def::TypeAlias(_)
                if matches!(
                    def.container(db),
                    Some(ItemContainerId::TraitId(_) | ItemContainerId::ImplId(_))
                ) =>
            {
                let Some(ItemContainerId::TraitId(it)) = def.container(db) else {
                    return false;
                };
                let trait_ = Def::Trait(it);
                let Some(parent) = trait_.name(db) else { return false };
                let parent = self.local_name(parent, trait_);
                self.write_name(&parent);
                self.write_str("::");
            }
            Def::Variant(it) => {
                let enum_ = Def::Adt(AdtId::EnumId(it.lookup(db).parent));
                let Some(parent) = enum_.name(db) else { return false };
                let parent = self.local_name(parent, enum_);
                self.write_name(&parent);
                self.write_str("::");
            }
            Def::TypeAlias(_) if def.is_foreign(db) => return false,
            Def::Adt(_)
            | Def::Trait(_)
            | Def::TypeAlias(_)
            | Def::Function(_)
            | Def::Const(_)
            | Def::Static(_) => {}
            _ => return false,
        }
        let name = self.local_name(name, def);
        self.write_name(&name);
        true
    }

    /// The name `def` is re-exported by where its visible parent is not its parent.
    fn local_name(&self, name: Name, def: Def) -> Name {
        let host = self.host();
        if let Some(visible) = host.visible_parent(def)
            && def.is_type_ns()
            && def::parent(self.db, def) != Some(Parent::Def(visible))
        {
            return host.reexport_name(visible, def).unwrap_or(name);
        }
        name
    }

    /// rustc's `try_print_visible_def_path_recur`: a dependency's item by the path its public
    /// re-exports reach it through from the crate root.
    fn try_print_visible(&mut self, def: Def, callers: &mut SmallVec<[Def; 8]>) -> bool {
        let db = self.db;
        let host = self.host();
        if let Def::Module(module) = def
            && def::parent(db, def).is_none()
        {
            let krate = module.krate(db);
            if krate != host.krate()
                && let Some(extern_crate) = host.extern_crate(krate)
            {
                let was = std::mem::replace(&mut self.no_visible, true);
                self.print_def_path(Def::ExternCrate(extern_crate), &[]);
                self.no_visible = was;
                return true;
            }
            self.print_crate_name(krate);
            return true;
        }
        if def.krate(db) == host.krate() {
            return false;
        }
        let Some(visible) = host.visible_parent(def) else { return false };
        let Some(mut name) = def.name(db) else { return false };
        if def.is_type_ns() && def::parent(db, def) != Some(Parent::Def(visible)) {
            match host.reexport_name(visible, def) {
                Some(it) => name = it,
                None => return false,
            }
        }
        if callers.contains(&visible) {
            return false;
        }
        callers.push(visible);
        if !self.try_print_visible(visible, callers) {
            return false;
        }
        callers.pop();
        self.path_segment(&name);
        true
    }

    /// rustc's `print_crate_name`: in a diagnostic nothing for the crate itself, else the
    /// crate's name.
    fn print_crate_name(&mut self, krate: base_db::Crate) {
        self.empty_path = true;
        if self.host.is_some_and(|host| host.krate() == krate) {
            return;
        }
        match &krate.extra_data(self.db).display_name {
            Some(name) => {
                let _ = write!(self.out, "{}", name.crate_name());
                self.empty_path = false;
            }
            None if self.diagnostic_spelling() => {
                panic!("a dependency is named by the crate graph")
            }
            None => self.refuse("a crate the crate graph gives no name"),
        }
    }

    /// rustc's `default_print_def_path`.
    fn default_print_def_path(&mut self, def: Def, args: &[GenericArg<'t>]) {
        if self.refused.is_some() {
            return;
        }
        if let Def::Impl(it) = def {
            return self.print_impl_path(it, args);
        }
        let db = self.db;
        let Some(parent) = def::parent(db, def) else {
            return self.print_unparented(def);
        };
        let mut parent_args = args;
        let mut qualify = false;
        if !args.is_empty() {
            let generics = self.generics_of(def, args.len());
            parent_args = &args[..generics.parent_count.min(args.len())];
            if generics.count > generics.parent_count && args.len() >= generics.count {
                let own = self.own_args_no_defaults(&generics, args);
                // A function's arguments hold only its early bound lifetimes, as rustc's
                // `generics_of` does (hir-def's `update_to_late_bound_lifetimes` is rustc's
                // `is_late_bound_map`), so `type_name` writes each of them `'_` and a late bound
                // one, which is no argument, not at all: `f<'_, u8>::{{closure}}`.
                self.print_def_path(def, parent_args);
                // A symbol has `::<..>` after a value's path, wherever it stands.
                let was = self.in_value;
                if self.mode.spelling == Spelling::Symbol {
                    self.in_value = def.is_value();
                }
                self.path_generic_args(own);
                self.in_value = was;
                return;
            }
            qualify = generics.has_self
                && generics.parent.is_some_and(|it| Parent::Def(it) == parent)
                && parent_args.len() == generics.parent_count;
        }
        match parent {
            // Debug info names no provided function of a trait by its type.
            Parent::Def(Def::Trait(_))
                if self.mode.spelling == Spelling::Debug && matches!(def, Def::Function(_)) =>
            {
                return self.refuse("a provided trait function in debug info");
            }
            Parent::Def(Def::Trait(it)) if qualify => {
                let tr = TraitRef::new_from_args(
                    self.interner,
                    it.into(),
                    GenericArgs::new_from_slice(parent_args),
                );
                self.print_path_with_qualified(tr.self_ty(), Some(tr));
            }
            parent => self.print_parent(&parent, parent_args),
        }
        // `type_name` writes an item of an `extern` block after the block.
        if def.is_foreign(db) {
            match self.mode.spelling {
                Spelling::Diagnostic => {}
                Spelling::TypeName => self.path_segment_fmt(format_args!("{{{{extern}}}}")),
                Spelling::Symbol | Spelling::Debug => {
                    return self.refuse("an item of an `extern` block in a symbol or debug info");
                }
            }
        }
        // None only for a block module (a crate root has no parent, an impl was printed
        // above): rustc has no definition for a block, its items sit under the body's owner,
        // which is the parent just printed.
        if let Some(name) = def.segment(db) {
            self.path_segment(&name);
        }
    }

    /// A definition `def::parent` gives no parent: a crate root, by its crate's name, or
    /// one written in a block whose body owner isn't found (a block in an array length of a
    /// signature, a discriminant: rustc's `{constant#N}`). A diagnostic prints it under the
    /// module the block is written in (rule panics-fail-one-job); other spellings refuse it.
    fn print_unparented(&mut self, def: Def) {
        let db = self.db;
        let root = matches!(def, Def::Module(module) if module.containing_module(db).is_none());
        if !root && !self.diagnostic_spelling() {
            return self.refuse(
                "an item or closure in an enum discriminant's expression or a signature's block: rustc \
                 names it after the `{constant#n}`, which is not numbered here",
            );
        }
        let Def::Module(module) = def else {
            self.print_unparented(Def::Module(def.module(db)));
            if let Some(name) = def.segment(db) {
                self.path_segment(&name);
            }
            return;
        };
        match module.containing_module(db) {
            Some(outer) if module.is_block_module(db) => {
                self.print_def_path(Def::Module(outer.to_static()), &[])
            }
            _ => self.print_crate_name(module.krate(db)),
        }
    }

    /// The path an item is printed under: its parent definition's, or its body owner's and
    /// the closures it is written in.
    pub(crate) fn print_parent(&mut self, parent: &Parent<'_>, args: &[GenericArg<'t>]) {
        match *parent {
            Parent::Def(def) => self.print_def_path(def, args),
            Parent::Body { in_anon_const: true, .. } if !self.diagnostic_spelling() => self.refuse(
                "an item or closure in an anonymous constant: rustc names it after the constant's \
                 `{constant#n}`, which is not numbered here",
            ),
            Parent::Body { owner, closures, .. } if self.mode.spelling == Spelling::Debug => {
                self.print_debug_owner(owner);
                self.closure_segments(closures);
            }
            Parent::Body { owner, closures, .. } => {
                self.print_def_path(owner, args);
                self.closure_segments(closures);
            }
        }
    }

    /// What debug info names the item a body is of, ahead of what is written in the body:
    /// a function by what it is written in ([`Printer::print_debug_container`]), a constant
    /// or static of a module by its path.
    fn print_debug_owner(&mut self, owner: Def) {
        let db = self.db;
        match (owner, owner.container(db)) {
            (Def::Function(_) | Def::Const(_), Some(ItemContainerId::ModuleId(_)))
            | (Def::Static(_), _) => {
                self.print_def_path(owner, &[]);
            }
            (Def::Function(func), _) => {
                let args = GenericArgs::identity_for_item(self.interner, func.into());
                self.print_debug_container(func, args.as_slice(), true);
                self.path_segment(&def_name(db, owner));
            }
            _ => self.refuse(NOT_LOOKED_UP),
        }
    }

    /// What a function is written in, as a debugger is told of it: its module; the type of an
    /// inherent impl of a type with a path when `inherent_as_type`, else `{impl#N}` by the
    /// impl's place in its module; or the trait.
    pub(crate) fn print_debug_container(
        &mut self,
        func: FunctionId,
        args: &[GenericArg<'t>],
        inherent_as_type: bool,
    ) {
        let db = self.db;
        match func.lookup(db).container {
            ItemContainerId::ModuleId(module) => self.print_def_path(Def::Module(module), &[]),
            ItemContainerId::ImplId(impl_) => {
                let parent_count = hir_ty::generics::generics(db, func.into()).len_parent();
                let Some(parent_args) = args.get(..parent_count) else {
                    return self.refuse(NOT_LOOKED_UP);
                };
                let self_ty = db
                    .impl_self_ty(impl_)
                    .instantiate(self.interner, parent_args)
                    .skip_normalization();
                if inherent_as_type
                    && db.impl_trait(impl_).is_none()
                    && matches!(self_ty.kind(), TyKind::Adt(..))
                {
                    self.print_type(self_ty);
                    self.empty_path = false;
                    return;
                }
                let module = impl_.lookup(db).container;
                let scope = module.def_map(db).module(db, module).scope(db);
                let Some(place) = scope.impls().position(|it| it == impl_) else {
                    return self.refuse(NOT_LOOKED_UP);
                };
                self.print_def_path(Def::Module(module), &[]);
                self.path_segment_fmt(format_args!("{{impl#{place}}}"));
            }
            ItemContainerId::TraitId(trait_) => self.print_def_path(Def::Trait(trait_), &[]),
            ItemContainerId::ExternBlockId(_) => {
                self.refuse("a function of an `extern` block in debug info")
            }
        }
    }

    /// rustc's `print_impl_path` and `default_print_impl_path`: `<S as Trait>` or `S` where
    /// the impl is in the module of its type or its trait, `module::<impl Trait for S>` else.
    /// Given fewer arguments than the impl has parameters, it is printed with its own, which
    /// `type_name` and debug info print `_`. A symbol always has `<S as Trait>` or `<S>`.
    fn print_impl_path(&mut self, it: hir_def::ImplId, args: &[GenericArg<'t>]) {
        let db = self.db;
        let generics = self.generics_of(Def::Impl(it), args.len());
        let self_ty = db.impl_self_ty(it);
        let trait_ref = db.impl_trait(it);
        let holes = generics.count > args.len();
        let (self_ty, trait_ref) = if !holes {
            (
                self_ty.instantiate(self.interner, args).skip_normalization(),
                trait_ref.map(|it| it.instantiate(self.interner, args).skip_normalization()),
            )
        } else {
            (
                self_ty.instantiate_identity().skip_normalization(),
                trait_ref.map(|it| it.instantiate_identity().skip_normalization()),
            )
        };
        if self.mode.spelling == Spelling::Symbol {
            self.generic_delimiters(|p| {
                p.print_type(self_ty);
                if let Some(tr) = trait_ref {
                    p.write_str(" as ");
                    p.print_trait_path(tr);
                }
            });
            self.empty_path = false;
            return;
        }
        let was = self.params_as_holes;
        self.params_as_holes = was || (holes && !self.diagnostic_spelling());
        let parent = def::parent(db, Def::Impl(it));
        let in_self_mod = matches!(self.characteristic_parent(self_ty), Some(Some(p)) if Some(&p) == parent.as_ref());
        let in_trait_mod =
            trait_ref.is_some_and(|tr| def::parent(db, Def::Trait(tr.def_id.0)) == parent);
        if !in_self_mod && !in_trait_mod {
            match &parent {
                Some(parent) => self.print_parent(parent, &[]),
                None if !self.diagnostic_spelling() => self.refuse(NOT_LOOKED_UP),
                None => {}
            }
            if !self.empty_path {
                self.write_str("::");
            }
            self.generic_delimiters(|p| {
                p.write_str("impl ");
                if let Some(tr) = trait_ref {
                    p.print_trait_path(tr);
                    p.write_str(" for ");
                }
                p.print_type(self_ty);
            });
            self.empty_path = false;
        } else {
            self.print_path_with_qualified(self_ty, trait_ref);
        }
        self.params_as_holes = was;
    }

    /// The parent of rustc's `characteristic_def_id_of_type`: Some(None) for a definition
    /// whose parent is never a module an impl is in (a constructor, a closure, a foreign type).
    fn characteristic_parent(&self, ty: Ty<'t>) -> Option<Option<Parent<'t>>> {
        let db = self.db;
        match ty.kind() {
            TyKind::Adt(def, _) => Some(def::parent(db, Def::Adt(def.def_id()))),
            TyKind::Dynamic(preds, _) => {
                let principal = preds.principal()?;
                Some(def::parent(db, Def::Trait(principal.skip_binder().def_id.0)))
            }
            TyKind::Pat(ty, _)
            | TyKind::Array(ty, _)
            | TyKind::Slice(ty)
            | TyKind::RawPtr(ty, _)
            | TyKind::Ref(_, ty, _) => self.characteristic_parent(ty),
            TyKind::Tuple(tys) => {
                tys.as_slice().iter().find_map(|it| self.characteristic_parent(*it))
            }
            TyKind::FnDef(def, _) => match def.0 {
                hir_def::CallableDefId::FunctionId(it) => Some(def::parent(db, Def::Function(it))),
                _ => Some(None),
            },
            TyKind::Closure(..)
            | TyKind::CoroutineClosure(..)
            | TyKind::Coroutine(..)
            | TyKind::CoroutineWitness(..)
            | TyKind::Foreign(_) => Some(None),
            _ => None,
        }
    }

    /// rustc's `pretty_print_path_with_qualified`: `S` for an inherent impl of a type with a
    /// path, `<S as Trait>` or `<&S>` else.
    pub(crate) fn print_path_with_qualified(
        &mut self,
        self_ty: Ty<'t>,
        trait_ref: Option<TraitRef<'t>>,
    ) {
        if trait_ref.is_none()
            && matches!(
                self_ty.kind(),
                TyKind::Adt(..)
                    | TyKind::Foreign(_)
                    | TyKind::Bool
                    | TyKind::Char
                    | TyKind::Str
                    | TyKind::Int(_)
                    | TyKind::Uint(_)
                    | TyKind::Float(_)
            )
        {
            self.print_type(self_ty);
            self.empty_path = false;
            return;
        }
        self.generic_delimiters(|p| {
            p.print_type(self_ty);
            if let Some(tr) = trait_ref {
                p.write_str(" as ");
                p.print_trait_path(tr);
            }
        });
        self.empty_path = false;
    }

    /// rustc's `print_only_trait_path`: `Trait<U>`.
    pub fn print_trait_path(&mut self, tr: TraitRef<'t>) {
        self.print_def_path(Def::Trait(tr.def_id.0), tr.args.as_slice());
    }

    /// rustc's `TraitRefPrintSugared`: `Fn(u8)` for a parenthesized trait, else the path.
    pub fn print_trait_sugared(&mut self, tr: TraitRef<'t>) {
        let trait_ = tr.def_id.0;
        let sugar = hir_def::signatures::TraitSignature::of(self.db, trait_)
            .flags
            .contains(hir_def::signatures::TraitFlags::RUSTC_PAREN_SUGAR);
        if sugar
            && let Some(args) = tr.args.as_slice().get(1).and_then(|it| it.ty())
            && let TyKind::Tuple(tys) = args.kind()
        {
            let name = Def::Trait(trait_).name(self.db).expect("a trait is named");
            self.write_name(&name);
            self.write_str("(");
            self.comma_sep_types(tys.as_slice());
            self.write_str(")");
        } else {
            self.print_trait_path(tr);
        }
    }

    /// rustc's `TraitPredicate` printing: `T: Trait`, `T: !Trait`.
    pub fn print_trait_predicate(&mut self, pred: TraitClause<'t>) {
        self.print_type(pred.trait_ref.self_ty());
        self.write_str(": ");
        if pred.polarity == ClausePolarity::Negative {
            self.write_str("!");
        }
        self.print_trait_sugared(pred.trait_ref);
    }

    /// rustc's `generics_of`, as far as paths read it; a variant has its enum's generics.
    pub fn generics_of(&self, def: Def, args: usize) -> Generics {
        let id: GenericDefId = match def {
            Def::Adt(it) => it.into(),
            Def::Function(it) => it.into(),
            Def::Const(it) => it.into(),
            Def::Static(it) => it.into(),
            Def::Trait(it) => it.into(),
            Def::TypeAlias(it) => it.into(),
            Def::Impl(it) => it.into(),
            Def::Variant(it) => {
                let enum_ = Def::Adt(AdtId::EnumId(it.lookup(self.db).parent));
                return Generics {
                    id: None,
                    parent: Some(enum_),
                    parent_count: args,
                    count: args,
                    has_self: false,
                };
            }
            Def::Module(_) | Def::Macro(_) | Def::ExternCrate(_) => {
                return Generics {
                    id: None,
                    parent: None,
                    parent_count: 0,
                    count: 0,
                    has_self: false,
                };
            }
        };
        let generics = hir_ty::generics::generics(self.db, id);
        let parent_count = generics.len_parent();
        let count = parent_count + generics.iter_self().count();
        let parent = match def.container(self.db) {
            Some(ItemContainerId::TraitId(it)) => Some(Def::Trait(it)),
            Some(ItemContainerId::ImplId(it)) => Some(Def::Impl(it)),
            _ => None,
        };
        let has_self = matches!(def, Def::Trait(_)) || matches!(parent, Some(Def::Trait(_)));
        Generics { id: Some(id), parent, parent_count, count, has_self }
    }

    /// rustc's `own_args_no_defaults`: the definition's own arguments, a trait's `Self` left
    /// out, without the trailing ones that are their parameter's default. Debug info keeps
    /// the defaults.
    pub fn own_args_no_defaults<'x>(
        &self,
        generics: &Generics,
        args: &'x [GenericArg<'t>],
    ) -> &'x [GenericArg<'t>] {
        let start =
            if generics.has_self && generics.parent.is_none() { 1 } else { generics.parent_count };
        let mut end = generics.count.min(args.len());
        if let Some(id) = generics.id
            && self.mode.spelling != Spelling::Debug
        {
            let defaults = self.db.generic_defaults(id);
            while end > generics.parent_count {
                let i = end - 1;
                match defaults.get(i) {
                    Some(default)
                        if default.instantiate(self.interner, args).skip_normalization()
                            == args[i] =>
                    {
                        end -= 1
                    }
                    _ => break,
                }
            }
        }
        &args[start.min(end)..end]
    }
}

fn def_name(db: &Db, def: Def) -> Name {
    def.segment(db).expect("a body's owner is named")
}

/// What rustc calls a closure of a kind in debug info.
pub(crate) fn kind_label(kind: ClosureKind) -> &'static str {
    use CoroutineKind::{Async, AsyncGen, Gen};
    use CoroutineSource::{Block, Closure, Fn};
    match kind {
        ClosureKind::Closure | ClosureKind::CoroutineClosure(_) => "closure",
        ClosureKind::OldCoroutine(_) => "coroutine",
        ClosureKind::Coroutine { kind: Async, source: Block } => "async_block",
        ClosureKind::Coroutine { kind: Async, source: Closure } => "async_closure",
        ClosureKind::Coroutine { kind: Async, source: Fn } => "async_fn",
        ClosureKind::Coroutine { kind: Gen, source: Block } => "gen_block",
        ClosureKind::Coroutine { kind: Gen, source: Closure } => "gen_closure",
        ClosureKind::Coroutine { kind: Gen, source: Fn } => "gen_fn",
        ClosureKind::Coroutine { kind: AsyncGen, source: Block } => "async_gen_block",
        ClosureKind::Coroutine { kind: AsyncGen, source: Closure } => "async_gen_closure",
        ClosureKind::Coroutine { kind: AsyncGen, source: Fn } => "async_gen_fn",
    }
}

/// The name of an early-bound lifetime parameter; None for one its function's parameters
/// leave out.
fn early_name(db: &Db, p: EarlyParamRegion) -> Option<Symbol> {
    let generics = GenericParams::of(db, p.id.parent);
    if p.id.local_id.into_raw().into_u32() as usize >= generics.len_lifetimes() {
        return None;
    }
    Some(*generics[p.id.local_id].name.symbol())
}

/// The name of a late-bound lifetime of a function: the `var`th late-bound lifetime parameter
/// of the definition the region names.
fn late_name(db: &Db, br: BoundRegion<'_>) -> Option<Symbol> {
    let BoundRegionKind::Named(def) = br.kind else { return None };
    let def = GenericDefId::try_from(def).ok()?;
    let (_, data) = GenericParams::of(db, def).iter_late_bound_lt().nth(br.var.as_usize())?;
    (data.name.as_str() != "'_").then(|| *data.name.symbol())
}

/// The regions a binder at `depth` binds, in the order rustc's `RegionFolder` reaches them.
struct BoundAt<'t> {
    depth: DebruijnIndex,
    found: SmallVec<[BoundRegion<'t>; 4]>,
}

impl<'t> TypeVisitor<DbInterner<'t>> for BoundAt<'t> {
    type Result = ();

    fn visit_binder<T: TypeVisitable<DbInterner<'t>>>(&mut self, t: &Binder<'t, T>) {
        self.depth.shift_in(1);
        t.super_visit_with(self);
        self.depth.shift_out(1);
    }

    fn visit_region(&mut self, r: Region<'t>) {
        if let RegionKind::ReBound(BoundVarIndexKind::Bound(d), br) = r.kind()
            && d == self.depth
            && !self.found.iter().any(|it| it.var == br.var)
        {
            self.found.push(br);
        }
    }
}

/// rustc's `RegionNameCollector`: the names of the regions in a value, which anonymous ones
/// are not given.
struct UsedNames<'t, 'x> {
    db: &'t Db,
    depth: DebruijnIndex,
    signature: bool,
    out: &'x mut HashSet<Symbol, Global>,
}

impl<'t> TypeVisitor<DbInterner<'t>> for UsedNames<'t, '_> {
    type Result = ();

    fn visit_binder<T: TypeVisitable<DbInterner<'t>>>(&mut self, t: &Binder<'t, T>) {
        self.depth.shift_in(1);
        t.super_visit_with(self);
        self.depth.shift_out(1);
    }

    fn visit_region(&mut self, r: Region<'t>) {
        let name = match r.kind() {
            RegionKind::ReEarlyParam(p) => early_name(self.db, p).filter(|it| it.as_str() != "'_"),
            RegionKind::ReLateParam(p) => late_name(self.db, p.kind),
            RegionKind::ReBound(BoundVarIndexKind::Bound(d), br)
                if self.signature && d == self.depth =>
            {
                late_name(self.db, br)
            }
            RegionKind::RePlaceholder(p) => late_name(self.db, p.bound),
            RegionKind::ReStatic => Some(Symbol::intern("'static")),
            _ => None,
        };
        if let Some(name) = name {
            self.out.insert(name);
        }
    }
}
