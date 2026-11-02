//! The printer: rustc's `FmtPrinter`, its regions, binders and generic arguments, and the
//! paths of definitions (`print_def_path`: trimmed, visible through re-exports, or the
//! definition's own path with its impl, trait and closures). Ported from
//! `compiler/rustc_middle/src/ty/print/pretty.rs`, `compiler/rustc_middle/src/ty/print/mod.rs`
//! and `own_args_no_defaults` of `compiler/rustc_middle/src/ty/generics.rs` (MIT or Apache-2.0).

use std::{fmt::Write as _, sync::LazyLock};

use base_db::Db;
use blaze_hash::HashSet;
use hir::db::HirDatabase;
use hir_def::{AdtId, GenericDefId, ItemContainerId, Lookup, hir::generics::GenericParams};
use hir_expand::name::Name;
use hir_ty::next_solver::{
    Binder, BoundRegion, DbInterner, EarlyParamRegion, GenericArg, GenericArgKind, GenericArgs,
    Region, RegionKind, TraitClause, TraitRef, Ty, TyKind,
};
use intern::Symbol;
use rustc_type_ir::{
    BoundRegionKind, BoundVarIndexKind, ClausePolarity, DebruijnIndex, INNERMOST,
    TypeSuperVisitable, TypeVisitable, TypeVisitor,
    inherent::{BoundExistentialPredicates as _, IntoKind as _},
};
use smallvec::SmallVec;

use super::paths::{self, Def, Parent, Paths};
use crate::cx::Cx;

/// rustc's printing flags: `with_forced_trimmed_paths!`, `with_no_trimmed_paths!`, regions
/// erased and anonymized first (`erase_and_anonymize_regions`), and the type length limit of
/// `FmtPrinter::new_with_limit`.
#[derive(Clone, Copy)]
pub(crate) struct Mode {
    pub(crate) forced: bool,
    pub(crate) untrimmed: bool,
    pub(crate) erase: bool,
    pub(crate) type_limit: Option<usize>,
}

impl Mode {
    pub(crate) const NORMAL: Mode = Mode { forced: false, untrimmed: false, erase: false, type_limit: None };
    pub(crate) const UNTRIMMED: Mode = Mode { untrimmed: true, ..Mode::NORMAL };
    pub(crate) const ERASED: Mode = Mode { erase: true, ..Mode::NORMAL };

    pub(crate) fn forced(self) -> Mode {
        Mode { forced: true, ..self }
    }

    pub(crate) fn limited(self, limit: usize) -> Mode {
        Mode { type_limit: Some(limit), ..self }
    }
}

/// rustc's `WrapBinderMode`, with a function item's signature told apart: its named bound
/// regions are the function's late-bound lifetimes, the names rustc prints for them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinderKind {
    ForAll,
    Signature,
    Unsafe,
}

/// The names given to the regions one binder binds, by bound variable.
struct Frame {
    names: SmallVec<[(u32, Symbol); 4]>,
}

/// rustc's names for anonymous regions, `'a` to `'z`.
static LETTERS: LazyLock<[Symbol; 26]> =
    // alloc: setup, once per process
    LazyLock::new(|| std::array::from_fn(|i| Symbol::intern(&format!("'{}", (b'a' + i as u8) as char))));

pub(crate) struct Printer<'p, 'a, 'db, 't> {
    pub(crate) cx: &'p Cx<'a, 'db>,
    pub(crate) db: &'t Db,
    pub(crate) interner: DbInterner<'t>,
    pub(crate) paths: &'p Paths<'a>,
    pub(crate) mode: Mode,
    pub(crate) in_value: bool,
    out: String,
    empty_path: bool,
    no_visible: bool,
    printed_type_count: usize,
    frames: Vec<Frame>,
    used_names: HashSet<Symbol>,
    region_index: usize,
}

/// What rustc's `generics_of` says of a definition, as path printing reads it.
pub(crate) struct Generics {
    pub(crate) id: Option<GenericDefId>,
    /// The trait or impl an associated item is in.
    pub(crate) parent: Option<Def>,
    pub(crate) parent_count: usize,
    pub(crate) count: usize,
    pub(crate) has_self: bool,
}

impl<'p, 'a, 'db: 't, 't> Printer<'p, 'a, 'db, 't> {
    pub(crate) fn new(cx: &'p Cx<'a, 'db>, mode: Mode, in_value: bool) -> Self {
        let db: &'t Db = cx.db;
        Printer {
            cx,
            db,
            interner: DbInterner::new_with(db, cx.krate.into()),
            paths: cx.paths(),
            mode,
            in_value,
            out: String::new(), // alloc: the text printed, the edge
            empty_path: false,
            no_visible: false,
            printed_type_count: 0,
            frames: Vec::new(), // alloc: empty, allocates nothing
            used_names: HashSet::default(),
            region_index: 0,
        }
    }

    pub(crate) fn finish(self) -> String {
        self.out
    }

    pub(crate) fn write_str(&mut self, s: &str) {
        self.out.push_str(s);
    }

    pub(crate) fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) {
        // Writing into a `String` does not fail.
        let _ = self.out.write_fmt(args);
    }

    /// An identifier, `r#` before one that is a keyword in the crate's edition.
    pub(crate) fn write_name(&mut self, name: &Name) {
        let edition = self.cx.edition;
        let _ = write!(self.out, "{}", name.display_no_db(edition));
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
    pub(crate) fn should_print_optional_region(&self, r: Region<'t>) -> bool {
        if self.mode.forced {
            return false;
        }
        match r.kind() {
            RegionKind::ReStatic => !self.mode.erase,
            _ => self.region_name(r).is_some(),
        }
    }

    /// rustc's `pretty_print_region`.
    pub(crate) fn print_region(&mut self, r: Region<'t>) {
        let kind = r.kind();
        if self.mode.erase && !matches!(kind, RegionKind::ReBound(..)) {
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
    /// `for<..> ` unless paths are forced trimmed, then `f` of the bound value.
    pub(crate) fn wrap_binder<T>(
        &mut self,
        binder: &Binder<'t, T>,
        kind: BinderKind,
        f: impl FnOnce(&mut Self, &T),
    ) where
        T: TypeVisitable<DbInterner<'t>>,
    {
        let value = binder.as_ref().skip_binder();
        if self.frames.is_empty() {
            let mut used = HashSet::default();
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

        let letters: SmallVec<[&Symbol; 26]> =
            // alloc: none, at most 26 inline in the `SmallVec`
            LETTERS.iter().filter(|it| !self.used_names.contains(*it)).collect();
        let mut taken = 0;
        let mut region_index = self.region_index;
        let mut names: SmallVec<[(u32, Symbol); 4]> = SmallVec::new();
        for br in bound.found {
            let own = match br.kind {
                BoundRegionKind::Named(_) if kind == BinderKind::Signature && !self.mode.erase => {
                    late_name(self.db, br)
                }
                BoundRegionKind::NamedForPrinting(name) if !self.mode.erase && name.0.as_str() != "'_" => Some(name.0),
                _ => None,
            };
            let name = own.unwrap_or_else(|| loop {
                let name = match letters.get(taken) {
                    Some(it) => {
                        taken += 1;
                        **it
                    }
                    // alloc: a region past `'z` named, printing a type, the edge
                    None => Symbol::intern(&format!("'z{}", region_index - letters.len())),
                };
                region_index += 1;
                if !self.used_names.contains(&name) {
                    break name;
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

    pub(crate) fn print_arg(&mut self, arg: GenericArg<'t>) {
        match arg.kind() {
            GenericArgKind::Lifetime(r) => self.print_region(r),
            GenericArgKind::Type(ty) => self.print_type(ty),
            GenericArgKind::Const(c) => self.print_const(c),
        }
    }

    pub(crate) fn comma_sep_args(&mut self, args: &[GenericArg<'t>]) {
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
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
    /// namespace, `<..>` else, nothing for none.
    pub(crate) fn path_generic_args(&mut self, args: &[GenericArg<'t>]) {
        if args.is_empty() {
            return;
        }
        if self.in_value {
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

    // Paths.

    /// rustc's `pretty_print_value_path`.
    pub(crate) fn print_value_path(&mut self, def: Def, args: &[GenericArg<'t>]) {
        let was = std::mem::replace(&mut self.in_value, true);
        self.print_def_path(def, args);
        self.in_value = was;
    }

    /// rustc's `FmtPrinter::print_def_path`.
    pub(crate) fn print_def_path(&mut self, def: Def, args: &[GenericArg<'t>]) {
        if args.is_empty() {
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
        let Some(name) = self.paths.trimmed(self.cx, def) else { return false };
        self.write_name(&name);
        true
    }

    /// rustc's `force_print_trimmed_def_path`: a type, trait or free item by its name, an
    /// associated item of a trait as `Trait::item`, a variant as `Enum::Variant`, or by its
    /// unique name.
    fn force_print_trimmed(&mut self, def: Def) -> bool {
        if let Def::Variant(_) = def
            && let Some(name) = self.paths.trimmed(self.cx, def)
        {
            let name = self.local_name(name, def);
            self.write_name(&name);
            return true;
        }
        let Some(name) = def.name(self.cx) else { return false };
        match def {
            Def::Function(_) | Def::Const(_) | Def::TypeAlias(_)
                if matches!(
                    def.container(self.cx),
                    Some(ItemContainerId::TraitId(_) | ItemContainerId::ImplId(_))
                ) =>
            {
                let Some(ItemContainerId::TraitId(it)) = def.container(self.cx) else {
                    return false;
                };
                let trait_ = Def::Trait(it);
                let Some(parent) = trait_.name(self.cx) else { return false };
                let parent = self.local_name(parent, trait_);
                self.write_name(&parent);
                self.write_str("::");
            }
            Def::Variant(it) => {
                let enum_ = Def::Adt(AdtId::EnumId(it.lookup(self.db).parent));
                let Some(parent) = enum_.name(self.cx) else { return false };
                let parent = self.local_name(parent, enum_);
                self.write_name(&parent);
                self.write_str("::");
            }
            Def::TypeAlias(_) if def.container(self.cx).is_some_and(|it| matches!(it, ItemContainerId::ExternBlockId(_))) => {
                return false;
            }
            Def::Adt(_) | Def::Trait(_) | Def::TypeAlias(_) | Def::Function(_) | Def::Const(_) | Def::Static(_) => {}
            _ => return false,
        }
        let name = self.local_name(name, def);
        self.write_name(&name);
        true
    }

    /// The name `def` is re-exported by where its visible parent is not its parent.
    fn local_name(&self, name: Name, def: Def) -> Name {
        if let Some(visible) = self.paths.visible_parent(self.cx, def)
            && def.is_type_ns()
            && paths::parent(self.cx, def) != Some(Parent::Def(visible))
        {
            return self.paths.reexport_name(self.cx, visible, def).unwrap_or(name);
        }
        name
    }

    /// rustc's `try_print_visible_def_path_recur`: a dependency's item by the path its public
    /// re-exports reach it through from the crate root.
    fn try_print_visible(&mut self, def: Def, callers: &mut SmallVec<[Def; 8]>) -> bool {
        if let Def::Module(module) = def
            && paths::parent(self.cx, def).is_none()
        {
            let krate = module.krate(self.db);
            if hir::Crate::from(krate) != self.cx.krate
                && let Some(extern_crate) = self.paths.extern_crate(self.cx, krate)
            {
                let was = std::mem::replace(&mut self.no_visible, true);
                self.print_def_path(Def::ExternCrate(extern_crate), &[]);
                self.no_visible = was;
                return true;
            }
            self.print_crate_name(krate);
            return true;
        }
        if def.is_local(self.cx) {
            return false;
        }
        let Some(visible) = self.paths.visible_parent(self.cx, def) else { return false };
        let Some(mut name) = def.name(self.cx) else { return false };
        if def.is_type_ns() && paths::parent(self.cx, def) != Some(Parent::Def(visible)) {
            match self.paths.reexport_name(self.cx, visible, def) {
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

    /// rustc's `print_crate_name`: nothing for the crate itself, else the crate's name.
    fn print_crate_name(&mut self, krate: base_db::Crate) {
        self.empty_path = true;
        let krate = hir::Crate::from(krate);
        if krate == self.cx.krate {
            return;
        }
        let name = krate.display_name(self.db).expect("a dependency is named by the crate graph");
        let _ = write!(self.out, "{}", name.crate_name());
        self.empty_path = false;
    }

    /// rustc's `default_print_def_path`.
    fn default_print_def_path(&mut self, def: Def, args: &[GenericArg<'t>]) {
        if let Def::Impl(it) = def {
            return self.print_impl_path(it, args);
        }
        let Some(parent) = paths::parent(self.cx, def) else {
            let Def::Module(module) = def else {
                unreachable!("only a crate root has no parent: {def:?}")
            };
            return self.print_crate_name(module.krate(self.db));
        };
        let mut parent_args = args;
        let mut qualify = false;
        if !args.is_empty() {
            let generics = self.generics_of(def, args.len());
            parent_args = &args[..generics.parent_count.min(args.len())];
            if generics.count > generics.parent_count && args.len() >= generics.count {
                let own = self.own_args_no_defaults(&generics, args);
                self.print_def_path(def, parent_args);
                self.path_generic_args(own);
                return;
            }
            qualify = generics.has_self
                && generics.parent.is_some_and(|it| Parent::Def(it) == parent)
                && parent_args.len() == generics.parent_count;
        }
        match parent {
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
        let name = def.name(self.cx).expect("a definition other than an impl or crate root is named");
        self.path_segment(&name);
    }

    /// The path an item is printed under: its parent definition's, or its body owner's and
    /// the closures it is written in.
    pub(crate) fn print_parent(&mut self, parent: &Parent, args: &[GenericArg<'t>]) {
        match parent {
            Parent::Def(def) => self.print_def_path(*def, args),
            Parent::Body { owner, closures } => {
                self.print_def_path(*owner, args);
                for index in closures {
                    if !self.empty_path {
                        self.write_str("::");
                    }
                    let _ = write!(self.out, "{{closure#{index}}}");
                    self.empty_path = false;
                }
            }
        }
    }

    /// rustc's `print_impl_path` and `default_print_impl_path`: `<S as Trait>` or `S` where
    /// the impl is in the module of its type or its trait, `module::<impl Trait for S>` else.
    fn print_impl_path(&mut self, it: hir_def::ImplId, args: &[GenericArg<'t>]) {
        let db = self.db;
        let generics = self.generics_of(Def::Impl(it), args.len());
        let self_ty = db.impl_self_ty(it);
        let trait_ref = db.impl_trait(it);
        let (self_ty, trait_ref) = if generics.count <= args.len() {
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
        let parent = paths::parent(self.cx, Def::Impl(it));
        let in_self_mod = matches!(self.characteristic_parent(self_ty), Some(Some(p)) if Some(&p) == parent.as_ref());
        let in_trait_mod = trait_ref.is_some_and(|tr| paths::parent(self.cx, Def::Trait(tr.def_id.0)) == parent);
        if !in_self_mod && !in_trait_mod {
            if let Some(parent) = &parent {
                self.print_parent(parent, &[]);
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
    }

    /// The parent of rustc's `characteristic_def_id_of_type`: Some(None) for a definition
    /// whose parent is never a module an impl is in (a constructor, a closure, a foreign type).
    fn characteristic_parent(&self, ty: Ty<'t>) -> Option<Option<Parent>> {
        match ty.kind() {
            TyKind::Adt(def, _) => Some(paths::parent(self.cx, Def::Adt(def.def_id()))),
            TyKind::Dynamic(preds, _) => {
                let principal = preds.principal()?;
                Some(paths::parent(self.cx, Def::Trait(principal.skip_binder().def_id.0)))
            }
            TyKind::Pat(ty, _) | TyKind::Array(ty, _) | TyKind::Slice(ty) | TyKind::RawPtr(ty, _) | TyKind::Ref(_, ty, _) => {
                self.characteristic_parent(ty)
            }
            TyKind::Tuple(tys) => tys.as_slice().iter().find_map(|it| self.characteristic_parent(*it)),
            TyKind::FnDef(def, _) => match def.0 {
                hir_def::CallableDefId::FunctionId(it) => Some(paths::parent(self.cx, Def::Function(it))),
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
    pub(crate) fn print_path_with_qualified(&mut self, self_ty: Ty<'t>, trait_ref: Option<TraitRef<'t>>) {
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
    pub(crate) fn print_trait_path(&mut self, tr: TraitRef<'t>) {
        self.print_def_path(Def::Trait(tr.def_id.0), tr.args.as_slice());
    }

    /// rustc's `TraitRefPrintSugared`: `Fn(u8)` for a parenthesized trait, else the path.
    pub(crate) fn print_trait_sugared(&mut self, tr: TraitRef<'t>) {
        let trait_ = tr.def_id.0;
        let sugar = hir_def::signatures::TraitSignature::of(self.db, trait_)
            .flags
            .contains(hir_def::signatures::TraitFlags::RUSTC_PAREN_SUGAR);
        if sugar
            && let Some(args) = tr.args.as_slice().get(1).and_then(|it| it.ty())
            && let TyKind::Tuple(tys) = args.kind()
        {
            let name = Def::Trait(trait_).name(self.cx).expect("a trait is named");
            self.write_name(&name);
            self.write_str("(");
            self.comma_sep_types(tys.as_slice());
            self.write_str(")");
        } else {
            self.print_trait_path(tr);
        }
    }

    /// rustc's `TraitPredicate` printing: `T: Trait`, `T: !Trait`.
    pub(crate) fn print_trait_predicate(&mut self, pred: TraitClause<'t>) {
        self.print_type(pred.trait_ref.self_ty());
        self.write_str(": ");
        if pred.polarity == ClausePolarity::Negative {
            self.write_str("!");
        }
        self.print_trait_sugared(pred.trait_ref);
    }

    /// rustc's `generics_of`, as far as paths read it; a variant has its enum's generics.
    pub(crate) fn generics_of(&self, def: Def, args: usize) -> Generics {
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
                return Generics { id: None, parent: Some(enum_), parent_count: args, count: args, has_self: false };
            }
            Def::Module(_) | Def::Macro(_) | Def::ExternCrate(_) => {
                return Generics { id: None, parent: None, parent_count: 0, count: 0, has_self: false };
            }
        };
        let generics = hir_ty::generics::generics(self.db, id);
        let parent_count = generics.len_parent();
        let count = parent_count + generics.iter_self().count();
        let parent = match def.container(self.cx) {
            Some(ItemContainerId::TraitId(it)) => Some(Def::Trait(it)),
            Some(ItemContainerId::ImplId(it)) => Some(Def::Impl(it)),
            _ => None,
        };
        let has_self = matches!(def, Def::Trait(_)) || matches!(parent, Some(Def::Trait(_)));
        Generics { id: Some(id), parent, parent_count, count, has_self }
    }

    /// rustc's `own_args_no_defaults`: the definition's own arguments, a trait's `Self` left
    /// out, without the trailing ones that are their parameter's default.
    pub(crate) fn own_args_no_defaults<'x>(
        &self,
        generics: &Generics,
        args: &'x [GenericArg<'t>],
    ) -> &'x [GenericArg<'t>] {
        let start = if generics.has_self && generics.parent.is_none() { 1 } else { generics.parent_count };
        let mut end = generics.count.min(args.len());
        if let Some(id) = generics.id {
            let defaults = self.db.generic_defaults(id);
            while end > generics.parent_count {
                let i = end - 1;
                match defaults.get(i) {
                    Some(default)
                        if default.instantiate(self.interner, args).skip_normalization() == args[i] =>
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
    out: &'x mut HashSet<Symbol>,
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
            RegionKind::ReBound(BoundVarIndexKind::Bound(d), br) if self.signature && d == self.depth => {
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
