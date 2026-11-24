//! The seam ([`crate::seam::FrontEnd`]) over the lazy program, by method group. A trait's impl
//! is one block, so the groups are its sections, each answering from the columns, the head
//! store, or the live bodies; none reads a table of every body.
//!
//! Groups: (1) rows and bodies: `fn_count`, `function`, `c_variadic`, `entry`, `types`, `sig`,
//! `fn_index`, `code_of`, `intrinsic`, `body_reads`, `check_all`, `check_one`, `done`;
//! (2) the body's world: `resolve`, `holds`, `normalize`, `last_projection`; (3) lang items:
//! `fn_lang`, `drop_trait`, `lang_item`, `lang_named`, `lang_trait_fn`, `fmt_argument_adt`,
//! `fmt_argument_new`; (4) types and items: `adt`, `repr`, `discriminant`, `ctor_of`,
//! `opaque_of`, `object_trait`, `item_named`, `left_out`; (5) literals: `int_lit`, `int_pat`,
//! `string`, `float_lit`, `float_pat`, `str_pat`, `format_template`; (6) constants and statics:
//! `const_record`, `const_at`, `evaluates_at_args`, `static_item`, `static_records`.

use super::*;

use crate::seam::FrontEnd;
use crate::front2::tables::recorder::TableRead;

/// All internal declarations in this program search the same graph. Keep its covering
/// reads once, and remember each caller that read them, even on a cached binding or refusal.
pub(super) struct InternalBindings<'n> {
    definitions: Map<ItemKey, ItemKey, Alloc<'n>>,
    callers: Map<ItemKey, (), Alloc<'n>>,
    reads: AVec<'n, TableRead>,
    /// A declaration first met at a ground instance. The seam carries Missing to make_asked,
    /// which can return the original reach refusal with its full payload.
    refused: Option<Front2Refusal>,
}

impl<'n> InternalBindings<'n> {
    pub(super) fn new_in(alloc: Alloc<'n>) -> Self {
        Self { definitions: Map::new_in(alloc), callers: Map::new_in(alloc), reads: AVec::new_in(alloc), refused: None }
    }

    fn read<S: NamesSource<'n>>(&mut self, owner: ItemKey, names: S) -> Result<(), Front2Refusal> {
        if self.reads.is_empty() {
            for krate in 0..names.crate_count() {
                let Some(key) = names.crate_key(krate) else {
                    self.reads.clear();
                    return Err(Front2Refusal::Unlisted { krate });
                };
                self.reads.push(crate::front2::internal_symbol::search_read(key));
            }
            self.reads.sort_unstable();
            self.reads.dedup();
        }
        self.callers.insert(owner, ());
        Ok(())
    }
}

impl<'n, S: PositionSource<'n>> LazyProgram<'n, S> {
    /// ADT `key`'s head is not made: a NEED, noted for the build's `make_asked` (as a world's
    /// question notes what it found unmade); the build's `NoAdt` makes it and asks again (run.rs
    /// `asks_again`). Only a key of a struct, an enum or a union is noted (by `kind_of_key`, the
    /// one reading of what a key names): a question about any other key is no need. The one
    /// home of the note for every head fact (`adt`, `repr`, `valid_range`, `discriminant`).
    fn head_unmade(&self, key: ItemKey) -> blaze_build::layout::Missing {
        use crate::front2::by_origin::{ItemKind, KeyKind, kind_of_key};
        use crate::front2::heads::Want;
        use crate::front2::world_items::Fill;
        if let Ok(KeyKind::Item(ItemKind::Adt(lib))) = kind_of_key(self.front.names, &self.front.keys, key) {
            let mut asked = self.asked.borrow_mut();
            if !asked.iter().any(|f| matches!(f, Fill::Store(Want::Adt(l)) if *l == lib)) {
                asked.push(Fill::Store(Want::Adt(lib)));
            }
        }
        blaze_build::layout::Missing { item: key }
    }

    /// `f` over the checker's world of row `i`'s live body: as `Front2Program::in_world`. A ctor
    /// (a row with no body, built as a fn) is asked in its ADT's world (`ctor_world`); any other
    /// row with no live body has no world: the question is the world's own refusal (`Missing`),
    /// naming the fn (the back half asks only in the body it builds, made by `check_one`).
    fn in_world<T>(&self, i: usize, f: impl FnOnce(&BodyWorld<'_, 'n, S>) -> T) -> Result<T, ResolveError> {
        let Some(live) = self.live_of(i) else {
            let key = self.rows.key[i];
            return crate::front2::world_items::ctor_world(self.front.names, &self.front.keys, &self.front.index, &self.store, self.buckets.as_ref(), key, &self.asked, &self.asked_gaps, f);
        };
        let body = &live.made;
        let view = body.lowered.view();
        // A function of the lowering alone, made once when the body was checked.
        let Ok(spell) = Spelling::of(&view) else {
            return Err(ResolveError::Missing(tc::Question::FnSig(self.rows.key[i])));
        };
        let scope = body.scope(&spell);
        let world = BodyWorld {
            names: self.front.names,
            keys: &self.front.keys,
            index: &self.front.index,
            store: &self.store,
            buckets: self.buckets.as_ref(),
            owner: scope.owner,
            module: scope.module,
            params: scope.params,
            self_: scope.self_,
            spell: scope.spell,
            traits: scope.traits,
            blocks: scope.blocks,
            path_marks: scope.path_marks,
            path_ns: scope.path_ns,
            fills: RefCell::new(Vec::new_in(Global)),
            gaps: RefCell::new(Vec::new_in(Global)),
            // As `Front2Program::in_world`: every question is at ground arguments after the
            // body checked (rustc's post-analysis mode), so every opaque is its hidden type.
            reveal: crate::front2::opaque_auto::Reveal::All,
        };
        let out = f(&world);
        // What the question found unmade, for the build's `make_asked`; what it found refused
        // (a candidate impl's header, by name), for `make_asked`'s refusal when nothing is
        // left to make: never the bare `Missing` (rule 19).
        self.asked.borrow_mut().extend(world.fills.into_inner());
        self.asked_gaps.borrow_mut().extend(world.gaps.into_inner());
        Ok(out)
    }

    fn lits_of(&self, owner: ItemKey) -> Option<&Literals> {
        self.live_by_key(owner).map(|l| &l.lits)
    }

    fn bind_internal(&self, owner: ItemKey, item: ItemKey) -> Result<Option<ItemKey>, Front2Refusal> {
        let names = self.front.names;
        let mut bindings = self.internal_bindings.borrow_mut();
        if let Some(&def) = bindings.definitions.get(&item) {
            bindings.read(owner, names)?;
            return Ok(Some(def));
        }
        let target = super::seam_rules::internal_binding(names, &self.front.keys, &self.store, item, || bindings.read(owner, names))?;
        if let Some(def) = target {
            bindings.definitions.insert(item, def);
        }
        Ok(target)
    }

    /// The caller's cross-crate reads for internal symbol binding.
    pub fn internal_binding_reads(&mut self, owner: ItemKey) -> &[TableRead] {
        let bindings = self.internal_bindings.get_mut();
        if bindings.callers.contains_key(&owner) { bindings.reads.as_slice() } else { &[] }
    }

    /// The seam's `check_one` with the body's checked columns read from the check's record
    /// (typed.rs), never checked here: the typing seam's `FromRecord` (run/make_one.rs). A
    /// gap is [`LazyRefusal::Typed`] naming the body.
    pub fn check_one_typed(&mut self, key: ItemKey, records: &mut dyn super::typed::TypedRecords) -> Result<Option<(usize, Checked<'static>)>, crate::run::RunError> {
        self.check_one_by(key, super::fill::Typer::Record(records))
    }

    /// `check_one` over either typer: the body made by `check_one_key_by`, then each fn it
    /// names bound (the reach walk also visits fns used as values, fn pointers included).
    fn check_one_by(&mut self, key: ItemKey, typer: super::fill::Typer<'_>) -> Result<Option<(usize, Checked<'static>)>, crate::run::RunError> {
        use crate::run::RunError;
        let body = self.check_one_key_by(key, typer).map_err(|e| RunError::Front2(Front2Refusal::Lazy(e)))?;
        if let Some((_, checked)) = &body {
            let values = checked.expr_defs.column::<1>().iter().filter_map(|d| match *d {
                tc::Def::Fn(k) => Some(k),
                _ => None,
            });
            for callee in checked.callees.column::<1>().iter().copied().chain(values) {
                self.bind_internal(key, callee).map_err(RunError::Front2)?;
            }
        } else {
            self.bind_internal(key, key).map_err(RunError::Front2)?;
        }
        Ok(body)
    }
}

impl<'n, S: PositionSource<'n>> FrontEnd for LazyProgram<'n, S> {
    // (1) Rows and bodies.

    fn fn_count(&self) -> usize {
        self.rows.len()
    }

    /// `body` is `Some` while `check_one` made it and `done` has not dropped it.
    fn function(&self, i: usize) -> FnFacts<'_> {
        let key = self.rows.key[i];
        FnFacts {
            key,
            path: self.rows.str(self.rows.path[i]),
            at: self.rows.at[i],
            text: self.store.fns.get(&key).map_or("", |h| h.text),
            kind: self.rows.kind[i],
            abi: self.rows.abi[i],
            symbol: self.rows.str(self.rows.symbol[i]),
            export: self.rows.export[i].map(|s| self.rows.str(s)),
            body: self.live_of(i).map(|l| (&l.made.typeck, &l.lits.texts)),
            const_fn: false,
            condition: false,
        }
    }

    fn c_variadic(&self, key: ItemKey) -> Option<u32> {
        let head = self.store.fns.get(&key)?;
        head.c_variadic.then_some(head.inputs.len() as u32)
    }

    fn legacy_const_generics(&self, key: ItemKey) -> &[u32] {
        self.store.fns.get(&key).map(|head| &head.legacy_const_generics[..]).unwrap_or(&[])
    }

    /// The entry's row once `check_one` made it.
    fn entry(&self) -> Option<usize> {
        self.entry_key.and_then(|k| self.rows.by_key.get(&k)).map(|&i| i as usize)
    }

    fn types(&self) -> PTys<'_> {
        self.head_types()
    }

    fn body_types(&self, i: usize) -> PTys<'_> {
        self.types_of_body(i)
    }

    fn resolve(&self, i: usize, item: ItemKey, args: List, ground: &mut PortableTys<'_>) -> Result<(ItemKey, List), ResolveError> {
        // As `Front2Program::resolve`, over this program's names source.
        let alloc = self.store.tys.alloc();
        if let Some(handler) = crate::front2::panic_entry::bound(self.front.names, &self.front.index, &self.store, item, alloc) {
            return Ok((handler, List::EMPTY));
        }
        if let Some(default) = crate::front2::alloc_shim::bound(self.front.names, &self.front.index, &self.store, item, alloc) {
            return Ok((default, List::EMPTY));
        }
        match self.bind_internal(self.rows.key[i], item) {
            Ok(Some(def)) => return Ok((def, List::EMPTY)),
            Ok(None) => {}
            Err(why) => {
                self.internal_bindings.borrow_mut().refused = Some(why);
                return Err(ResolveError::Missing(tc::Question::Attrs(item)));
            }
        }
        let r = self.in_world(i, |w| resolve_instance(w, item, args, ground, alloc))??;
        Ok((r.item, r.args))
    }

    fn holds(&self, i: usize, trait_: ItemKey, ty: PTy, ground: &mut PortableTys<'_>) -> Result<bool, ResolveError> {
        let alloc = self.store.tys.alloc();
        self.in_world(i, |w| tc::holds(w, trait_, &[ty], ground, alloc))?
    }

    fn adt_drop_impl(&self, i: usize, drop_trait: ItemKey, adt: ItemKey, alloc: Alloc<'_>) -> Result<Option<ItemKey>, ResolveError> {
        self.in_world(i, |w| blaze_typeck::adt_drop_impl(w, drop_trait, adt, alloc))?
    }

    /// As `Front2Program::make_asked`: the fills the instance questions' worlds noted, made
    /// as the body check makes its own (check.rs `make_all`).
    fn make_asked(&mut self, i: usize) -> Result<bool, crate::run::RunError> {
        if let Some(why) = self.internal_bindings.get_mut().refused.take() {
            return Err(crate::run::RunError::Front2(why));
        }
        let fills = core::mem::replace(self.asked.get_mut(), Vec::new_in(Global));
        let mut gaps = core::mem::replace(self.asked_gaps.get_mut(), Vec::new_in(Global));
        let owner = self.rows.key[i];
        let function: Box<str> = Box::from(self.rows.str(self.rows.path[i]));
        if fills.is_empty() {
            // Nothing to make: a question the world refused (a refused candidate impl) is the
            // instance's refusal by name, never the bare "no answer" (rule 19); none at all is
            // `Ok(false)`, the caller's refusal as before.
            if gaps.is_empty() {
                return Ok(false);
            }
            gaps.sort_unstable_by_key(|g| std::format!("{g:?}"));
            gaps.dedup_by_key(|g| std::format!("{g:?}"));
            let error = crate::front2::check::CheckFail::Gap { owner, gaps };
            return Err(crate::run::RunError::Front2(crate::front2::program::Front2Refusal::Asked { function, error }));
        }
        // With the program's evaluator, as a body check makes its world's fills (fill.rs
        // `records(true)`): a head these fills make may name a constant (an impl's `type Output
        // = [u32; BUFFER_SIZE]`, chacha20 rng.rs; a parameter `[u8; size_of::<Self>()]` of an
        // integer's method; ruint's `Uint<256, { nlimbs!(256) }>`), and the Reference,
        // "Constant evaluation", "Const context": "Array type length expressions" and "Const
        // generic arguments" are evaluated as a constant item is, reading other constants to
        // any depth. Without it check.rs `eval_len` found no evaluator and every such fill was
        // `ConstFail::Nested` (or an anonymous constant filed as "this caller has no constant
        // evaluator"): the instance refused. Never called inside an evaluation (`const_at` is
        // one seam call with the evaluator taken there), so this is no nested evaluation.
        let mut records = Records {
            names: self.front.names,
            keys: &mut self.front.keys,
            index: &self.front.index,
            store: &mut self.store,
            buckets: &mut self.buckets,
            order: BuildOrder::AsAsked,
            consts: Some((&mut self.consts, self.prepared)),
        };
        crate::front2::check::make_all(&mut records, fills, owner)
            .map_err(|error| crate::run::RunError::Front2(crate::front2::program::Front2Refusal::Asked { function, error }))?;
        Ok(true)
    }

    /// Every row has its head in the store or is a ctor: `row` makes no row otherwise.
    fn sig(&self, i: usize) -> FnSig<'_> {
        let key = self.rows.key[i];
        let types = self.types();
        if let Some(c) = self.store.ctors.get(&key) {
            let header = tc::FnPtrHeader::RUST;
            return FnSig { parent_count: 0, own_count: c.generics, declared_count: c.generics, self_param: false, header, c_variadic: false, legacy_const_generics: &[], inputs: &c.inputs[..], output: c.output, types };
        }
        let f = &self.store.fns[&key];
        FnSig {
            parent_count: f.parent,
            own_count: f.own,
            declared_count: f.declared,
            self_param: f.self_param,
            header: f.header,
            c_variadic: false,
            legacy_const_generics: &[],
            inputs: &f.inputs,
            output: self.store.fn_output(key).unwrap_or(f.output),
            types,
        }
    }

    /// From the fn's row (fill.rs `row`): its written ABI's unwind category (program/head_abi.rs
    /// `BuildAbi`: "C"/"system" nounwind, "Rust"/"rust-call"/"-unwind" not) or
    /// `#[rustc_nounwind]`, as `Front2Program`'s, falling back to the signature when no row is
    /// made yet.
    fn nounwind(&self, key: ItemKey) -> Option<bool> {
        let tabled = self.rows.by_key.get(&key).and_then(|&i| self.rows.nounwind.get(i as usize).copied());
        super::seam_rules::nounwind_from(tabled, self.front.names, &self.front.keys, &self.store, key)
    }

    fn fn_attrs(&self, key: ItemKey) -> blaze_lir::func::FnAttrs {
        self.rows
            .by_key
            .get(&key)
            .and_then(|&i| usize::try_from(i).ok().and_then(|idx| self.rows.fn_attrs.get(idx).copied()))
            .unwrap_or_default()
    }

    /// A row's ABI, else the head's (program/seam_rules.rs `fn_abi`, `Front2Program`'s rule): a
    /// callee the build resolved to before its own `check_one` made its row still has its
    /// signature's ABI.
    fn fn_abi(&self, key: ItemKey) -> Option<Abi> {
        // The ABI of the fn an allocator declaration runs, not the declaration's (program.rs
        // `fn_abi`, blaze-build `Adts::abi`).
        let key = crate::front2::alloc_shim::bound(self.front.names, &self.front.index, &self.store, key, self.store.tys.alloc()).unwrap_or(key);
        let tabled = self.rows.by_key.get(&key).and_then(|&i| self.rows.abi.get(i as usize).copied());
        super::seam_rules::fn_abi(tabled, self.front.names, &self.store, key)
    }

    /// Read a filed row's attribute, or ask the shared rule for the function's signature.
    fn track_caller(&self, key: ItemKey) -> Option<bool> {
        super::seam_rules::track_caller_from(self.front.names, &self.front.keys, &self.store, key, |k| {
            self.rows.track_caller.get(*self.rows.by_key.get(&k)? as usize).copied()
        })
    }

    /// From `owner`'s live body (the build asks while it builds that body) by program/
    /// seam_rules.rs `caller_site`: item-relative, the file by its shown path from the names
    /// source (over records, the items record's file row).
    fn caller_site(&self, owner: ItemKey, expr: ExprId) -> Result<blaze_build::CallerSite<'_>, blaze_build::SiteGap> {
        let live = self.live_by_key(owner).ok_or(blaze_build::SiteGap::NotLive { owner })?;
        let anchor_owner = live.made.param_subst.as_ref().map_or(owner, |(orig, _)| *orig);
        super::seam_rules::caller_site(self.front.names, &self.front.keys, &self.store, anchor_owner, &live.made, expr)
    }

    /// By program/seam_rules.rs `def_site`, from `item`'s head and the names source: no body
    /// is read (a `Fn*` method has none).
    fn def_site(&self, item: ItemKey) -> Result<blaze_build::CallerSite<'_>, blaze_build::SiteGap> {
        let anchor = self.spec_orig(item).unwrap_or(item);
        super::seam_rules::def_site(self.front.names, &self.front.keys, &self.store, anchor)
    }

    /// From `owner`'s live body by program/seam_rules.rs `expr_offset`: no file asked, so a
    /// toolchain crate's trap keeps its site.
    fn expr_site(&self, owner: ItemKey, expr: ExprId) -> Option<u32> {
        let live = self.live_by_key(owner)?;
        super::seam_rules::expr_offset(&live.made, expr)
    }

    /// program/seam_rules.rs `source_position`: the item's first byte today and today's text of
    /// its file, from the names source (over records, item_grain.rs `row_now` and `file_now`).
    fn source_position(&self, item: ItemKey, site: u32) -> Result<(u32, u32), crate::seam::PositionGap> {
        super::seam_rules::source_position(self.front.names, &self.front.keys, &self.store, item, site)
    }

    /// The start [`LazyProgram::decide_start`] decided, when it is a Rust start.
    fn rust_start(&self) -> Option<&crate::front2::start::RustStart> {
        match &self.start {
            Some(crate::front2::start::Entry2::Rust(s)) => Some(s),
            _ => None,
        }
    }

    /// The recipe made with the owner's check (fill.rs `check_one_key`), as
    /// `Front2Program::coroutine_recipe`. `None`: the owner was not checked, or has no such block.
    fn coroutine_recipe(&self, owner: ItemKey, ordinal: u32) -> Option<Result<&blaze_flow_lower::state_recipe::StateRecipe<Global>, blaze_flow_lower::Refusal>> {
        crate::front2::coroutine_recipes::find(&self.recipes, owner, ordinal)
    }

    /// Only rows already made: a row is added by `check_one` alone (module docs).
    fn fn_index(&self, key: ItemKey) -> Option<usize> {
        self.rows.by_key.get(&key).map(|&i| i as usize)
    }

    fn specialized_row(&self, i: usize, args: &[PTy], ground: PTys<'_>) -> Option<usize> {
        if self.inline_for_specs.is_empty() {
            return None;
        }
        let orig = *self.rows.key.get(i)?;
        let alloc = self.store.tys.alloc();
        for s in self.inline_for_specs.iter() {
            if s.orig == orig && crate::front2::inline_for::args_equal(ground, args, self.store.tys.view(), &s.args, alloc) {
                return Some(s.row);
            }
        }
        None
    }

    fn specialize_inline_for(&mut self, i: usize, args: &[PTy], ground: PTys<'_>) -> Result<Option<(usize, Option<Checked<'static>>)>, crate::run::RunError> {
        if let Some(existing) = self.specialized_row(i, args, ground) {
            return Ok(Some((existing, None)));
        }
        let Some((spec_row, orig_key, checked)) = self
            .specialize_inline_for_row(i, args, ground)
            .map_err(|e| crate::run::RunError::Front2(Front2Refusal::Lazy(e)))?
        else {
            return Ok(None);
        };
        let values = checked.expr_defs.column::<1>().iter().filter_map(|d| match *d {
            tc::Def::Fn(k) => Some(k),
            _ => None,
        });
        for callee in checked.callees.column::<1>().iter().copied().chain(values) {
            self.bind_internal(orig_key, callee).map_err(crate::run::RunError::Front2)?;
        }
        Ok(Some((spec_row, Some(checked))))
    }

    /// Over the source's questions: compiler_builtins' item rows, then the signatures of the
    /// fns named a routine's symbol (front2/builtins_shim.rs).
    fn soft_float_routines(&self, alloc: blaze_own::Alloc<'_>) -> crate::front2::builtins_shim::Builtins {
        crate::front2::builtins_shim::find(self.front.names, self.front.index.scope(), alloc)
    }

    fn body_reads(&self, i: usize, _checked: &Checked<'static>) -> Option<crate::front2::body_reads::ReadsDigest> {
        self.store.reads.get(self.rows.key[i])
    }

    /// Never: the lazy program checks one body at a time (`check_one`).
    fn check_all(&mut self) -> Result<Vec<Option<Checked<'static>>, Global>, crate::run::RunError> {
        Err(crate::run::RunError::Front2(Front2Refusal::Lazy(LazyRefusal::NotLazyYet { method: "check_all", item: Box::from("the whole program") })))
    }

    fn check_one(&mut self, key: ItemKey) -> Result<Option<(usize, Checked<'static>)>, crate::run::RunError> {
        self.check_one_by(key, super::fill::Typer::Check)
    }

    fn head_row(&mut self, key: ItemKey) -> Result<Option<usize>, crate::run::RunError> {
        self.head_row_key(key).map(Some).map_err(|e| crate::run::RunError::Front2(Front2Refusal::Lazy(e)))
    }

    fn done(&mut self, i: usize) {
        self.done_row(i);
    }

    fn intrinsic(&self, key: ItemKey) -> Option<Intrinsic> {
        self.rows.intrinsic[*self.rows.by_key.get(&key)? as usize]
    }

    /// As `Front2Program::code_of`, its text over the source.
    fn code_of(&self, i: usize) -> ContentId {
        let owner = self.rows.key[i];
        let text = match (self.store.locals.head(owner), self.front.keys.library_in(self.front.names, owner)) {
            (Some(local), _) => Some(local.text),
            (None, Some(lib)) => self.front.names.text_of(lib).map(|t| t.text),
            (None, None) => self.store.fns.get(&owner).map(|h| h.text),
        };
        crate::front2::consts::item_code_id(owner, text)
    }

    // (2) The body's world.

    /// In the first live body's world: selection and the projection step read the impls and
    /// the traits, never a body's own scope (as `Front2Program::normalize`).
    /// With no live body (a found code entry's read list asked again before any check:
    /// run/make_one.rs `verify_instance`), the first row's world (world_items.rs `ctor_world`:
    /// a fn row's module), for the same reason.
    fn normalize(&self, trait_: ItemKey, name: tc::Name, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<Option<PTy>, ResolveError> {
        let i = match self.live.iter().flatten().map(|l| l.row as usize).min() {
            Some(i) => i,
            None if !self.rows.key.is_empty() => 0,
            None => {
                *self.projection_why.borrow_mut() = Some(Box::from("the lazy program holds no row to ask in"));
                return Ok(None);
            }
        };
        let alloc = self.store.tys.alloc();
        let out = self.in_world(i, |w| blaze_typeck::normalize_projection(w, trait_, name, args, ground, alloc))?;
        if !matches!(out, Ok(Some(_))) {
            *self.projection_why.borrow_mut() = Some(crate::front2::program::projection_why(self, trait_, name, args, ground, &out));
        }
        out
    }

    fn last_projection(&self) -> Option<Box<str>> {
        self.projection_why.borrow_mut().take()
    }

    // (3) Lang items.

    fn fn_lang(&self, l: FnLang) -> Option<ItemKey> {
        self.front.index.fn_lang(l).map(checker_of)
    }

    fn drop_trait(&self) -> Option<(ItemKey, Option<ItemKey>)> {
        let tr = self.made_lang(LangItem::Drop)?;
        Some((tr, self.front.keys.filed_member(tr, MemberKind::Fn, "drop")))
    }

    fn lang_item(&self, item: LangItem) -> Result<Option<ItemKey>, crate::seam::LangRefusal> {
        // A lang member (`Future::poll`) by the key its container's head files it under
        // (keys.rs `asked_key`), so the build's call and the head the program makes are one key.
        let names = self.front.names;
        match crate::front2::langs::find(&self.front.index, self.front.resolver, item) {
            Ok(found) => Ok(found.map(|lib| crate::front2::keys::asked_key(names, lib))),
            Err(why) => Err(crate::seam::LangRefusal { item, why }),
        }
    }

    fn lang_named(&self, name: &str) -> Option<ItemKey> {
        let names = self.front.names;
        self.front.index.lang(name).map(|lib| crate::front2::keys::asked_key(names, lib))
    }

    /// The method `name` of lang trait `trait_` by the key its trait's head files it under
    /// (keys.rs `member_key_of`), from the library's member row when the head is not made yet:
    /// the lazy program makes a head only when a checked body names the trait, and a closure
    /// coerced to a fn pointer in a constant (`const C: fn(u8) -> u8 = |x| x + 1;`) is built
    /// through `Fn::call` though no body of the program names `Fn` (the build of its
    /// `call_once` entry, blaze-build closure.rs `body_instance`). The key is the one the head
    /// files later, so the build's call and a later check name one item, as `lang_item` does for
    /// a `#[lang]` member. `None`: the trait declares no fn `name`.
    fn lang_trait_fn(&self, trait_: LangItem, name: &str) -> Option<ItemKey> {
        let tr = self.made_lang(trait_)?;
        if let Some(filed) = self.front.keys.filed_member(tr, MemberKind::Fn, name) {
            return Some(filed);
        }
        let names = self.front.names;
        let lib = self.front.keys.library_in(names, tr)?;
        let mut declared = false;
        names.each_member(lib, &mut |m| declared |= m.kind == blaze_surface::ItemKind::Function && m.name == Some(name));
        declared.then(|| crate::front2::keys::member_key_of(tr, MemberKind::Fn, name))
    }

    fn fmt_argument_adt(&self) -> Option<ItemKey> {
        self.front.index.lang("format_argument").map(checker_of)
    }

    /// As `Front2Program::fmt_argument_new`: the answer `check_one` asked for the body's
    /// placeholders (fill.rs `fmt_rows`), over the solved names or the records alike.
    fn fmt_argument_new(&self, trait_: tc::FmtTrait) -> Option<ItemKey> {
        crate::front2::fmt_template::argument_new_answered(&self.front.index, self.buckets.as_ref(), trait_)
    }

    // (4) Types and items.

    /// An ADT whose row is not made yet (a type the build met that no checked body's types
    /// named: a field's type grounded at an instance's arguments, a projection normalized at
    /// them) is a NEED, not a refusal: its head is noted for the build's `make_asked`, as a
    /// world's question notes what it found unmade, and the build's `NoAdt` makes it and builds
    /// again (run.rs `asks_again`). Only a key of a struct, an enum or a union is noted (by
    /// `kind_of_key`, the one reading of what a key names): a question about any other key is
    /// no need, and stays unanswered.
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        let Some(a) = self.store.adts.get(&key) else {
            self.head_unmade(key);
            return None;
        };
        // The head store's own table, which holds every head's types: `types()` is a fork of it
        // taken at the last `check_one`, which lacks a row made after (by `make_asked`), and
        // agrees with it on every row made before. Every reader grounds a field's type from
        // `Adt::types` alone (blaze-build `ground`).
        Some(Adt { kind: a.kind, generics: a.generics, variants: &a.variants, fields: &a.fields, types: self.store.tys.view() })
    }

    /// The row's `#[repr(..)]`. No row yet: a NEED, as [`Self::adt`]'s (its head noted for
    /// `make_asked`), answered `Missing`, never a default `repr(Rust)`: a packed, `simd` or
    /// niche-hiding ADT asked before its head would otherwise be laid out by what was asked
    /// first (D194).
    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, blaze_build::layout::Missing> {
        match self.store.adts.get(&key) {
            Some(a) => Ok(a.repr),
            None => Err(self.head_unmade(key)),
        }
    }

    /// The row's valid range; no row yet: `Missing`, its head noted, as [`Self::repr`].
    fn valid_range(&self, key: ItemKey) -> Result<blaze_build::layout::ValidRange, blaze_build::layout::Missing> {
        match self.store.adts.get(&key) {
            Some(a) => Ok(a.valid),
            None => Err(self.head_unmade(key)),
        }
    }

    /// The row's value of the variant; no row yet: `Missing`, its head noted, as
    /// [`Self::repr`] (never `None`, which numbers the variants by index).
    fn discriminant(&self, adt: ItemKey, variant: u32) -> Result<Option<i128>, blaze_build::layout::Missing> {
        match self.store.adts.get(&adt) {
            Some(a) => Ok(a.discrs.get(variant as usize).copied()),
            None => Err(self.head_unmade(adt)),
        }
    }

    fn ctor_of(&self, key: ItemKey) -> Option<(ItemKey, u32)> {
        let adt = self.store.ctors.get(&key)?.adt;
        let variant = self.store.adts.get(&adt)?.variants.iter().position(|v| v.key == key)?;
        Some((adt, variant as u32))
    }

    fn opaque_of(&self, opaque: ItemKey) -> Option<(ItemKey, u32)> {
        self.store.opaques.get(&opaque).copied()
    }

    fn alias_definer(&self, alias: ItemKey) -> Result<ItemKey, crate::front2::tait::DefinerMiss> {
        let names = self.front.names;
        let lib = self.front.keys.library_in(names, alias).ok_or(crate::front2::tait::DefinerMiss::NoLibraryKey)?;
        crate::front2::tait::defining_fn(&self.definers, names, lib).map(crate::front2::keys::checker_of)
    }

    /// The table made for a principal the checked bodies name (fill.rs `body_objects`, a
    /// prefetch) or that a build asked for by name ([`FrontEnd::object_table`]), as
    /// `Front2Program::object_trait`.
    fn object_trait(&self, t: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>> {
        let o = self.objects.get(&t)?;
        let tys = o.super_tys.as_ref().map_or(PTys::EMPTY, PortableTys::view);
        Some(blaze_build::vtable::ObjectTrait { supers: &o.supers, tys, methods: &o.methods, shape: o.shape })
    }

    /// fill.rs `object_asked`: `t`'s table from its head and its supertraits' heads, so an
    /// instance's vtable never waits on another body's check (D194).
    fn object_table(&mut self, t: ItemKey) -> Result<bool, crate::run::RunError> {
        self.object_asked(t).map_err(|e| crate::run::RunError::Front2(Front2Refusal::Lazy(e)))
    }

    /// fill.rs `site_heads_asked`: each head enclosing a body-local owner, made before its build
    /// (D194).
    fn site_heads(&mut self, owner: ItemKey) -> Result<(), crate::run::RunError> {
        self.site_heads_asked(owner).map_err(|e| crate::run::RunError::Front2(Front2Refusal::Lazy(e)))
    }

    /// `crate::module::Name`, over the source's questions (module path, name).
    fn item_named(&self, key: ItemKey) -> Option<Box<str>> {
        // The one printer of an item (front2 item_shown.rs, rule 30): a member by its trait's or
        // impl's path (an impl by its header, `<impl Display for Arguments<'_>>`, which this
        // printed as no path before: every fn of an impl), a variant by its enum's.
        self.item_shown(key).path().map(Box::from)
    }

    /// A run leaves nothing out: a fn that does not check is `check_one`'s refusal.
    fn left_out(&self, _key: ItemKey) -> Option<&str> {
        None
    }

    fn type_names(&self) -> Option<&dyn blaze_build::type_name::TypeNames> {
        Some(self)
    }

    // (5) Literals: of the live body of `owner`.

    fn int_lit(&self, owner: ItemKey, e: ExprId) -> Option<u128> {
        let s = &self.lits_of(owner)?.ints;
        s.binary_search_by_key(&e.0, |(x, _)| x.0).ok().map(|i| s[i].1)
    }

    fn int_pat(&self, owner: ItemKey, p: PatId) -> Option<u128> {
        let s = &self.lits_of(owner)?.int_pats;
        s.binary_search_by_key(&p.0, |(x, _)| x.0).ok().map(|i| s[i].1)
    }

    fn string(&self, owner: ItemKey, e: ExprId) -> Option<&[u8]> {
        let s = &self.lits_of(owner)?.strings;
        s.binary_search_by_key(&e.0, |(x, _)| x.0).ok().map(|i| &*s[i].1)
    }

    fn float_lit(&self, owner: ItemKey, e: ExprId) -> Option<(u64, u32)> {
        let s = &self.lits_of(owner)?.floats;
        s.binary_search_by_key(&e.0, |(x, _)| x.0).ok().map(|i| s[i].1)
    }

    fn float_pat(&self, owner: ItemKey, p: PatId) -> Option<(u64, u32)> {
        let s = &self.lits_of(owner)?.float_pats;
        s.binary_search_by_key(&p.0, |(x, _)| x.0).ok().map(|i| s[i].1)
    }

    fn str_pat(&self, owner: ItemKey, p: PatId) -> Option<&[u8]> {
        let s = &self.lits_of(owner)?.str_pats;
        s.binary_search_by_key(&p.0, |(x, _)| x.0).ok().map(|i| &*s[i].1)
    }

    fn format_template(&self, owner: ItemKey, e: ExprId) -> Option<&[blaze_build::FmtPiece<'_>]> {
        let s = &self.lits_of(owner)?.templates;
        s.binary_search_by_key(&e.0, |(x, _)| x.0).ok().map(|i| s[i].1.pieces())
    }

    // (6) Constants and statics.

    fn const_record(&self, item: ItemKey) -> Option<&DataRecord<Global>> {
        let at = self.const_values.binary_search_by_key(&item, |(k, _)| *k).ok()?;
        Some(&self.const_values[at].1)
    }

    /// As `Front2Program::const_at`: the evaluator the program keeps checks any body and makes
    /// any impl row the value needs.
    fn const_at(&mut self, need: crate::front2::const_at::ConstNeed, from: tc::PTys<'_>, args: &[tc::PTy], closures: &crate::run::ClosureTable) -> Result<DataRecord<Global>, crate::front2::const_at::ConstAtWhy> {
        let prepared = self.prepared;
        let alloc = self.store.tys.alloc();
        let mut records = Records {
            names: self.front.names,
            keys: &mut self.front.keys,
            index: &self.front.index,
            store: &mut self.store,
            buckets: &mut self.buckets,
            order: BuildOrder::AsAsked,
            consts: None,
        };
        let made = match need {
            crate::front2::const_at::ConstNeed::Item(item) => self.consts.value_with_args(&mut records, prepared, item, from, args, Some(closures), alloc),
            crate::front2::const_at::ConstNeed::Block { .. } | crate::front2::const_at::ConstNeed::Promoted { .. } => {
                let owner = need.item();
                let in_body = records.store.fns.get(&owner).and_then(|h| need.in_body(h.text));
                let Some((owner, root, key)) = in_body else {
                    return Err(crate::front2::const_at::ConstAtWhy::Eval(crate::front2::consts::ConstFail::Body { item: owner, error: BodyError::NoHead(owner) }));
                };
                self.consts.body_value(&mut records, prepared, owner, root, key, from, args, Some(closures), alloc)
            }
        };
        made.map(crate::front2::const_at::data_record).map_err(crate::front2::const_at::ConstAtWhy::Eval)
    }

    /// As `Front2Program::const_fn_instance`: the record of the evaluator the program keeps
    /// names the instance of each of its fn words (consts.rs `Consts::fn_instance`), whether the
    /// value was made when its reader's body was checked (`const_record`) or pulled at an
    /// instance's arguments (`const_at`).
    fn const_fn_instance(&self, key: blaze_build::InstanceKey, ground: &mut tc::PortableTys<'_>, scratch: blaze_own::Alloc<'_>) -> Option<blaze_build::Instance> {
        self.consts.fn_instance(key, ground, scratch)
    }

    fn evaluates_at_args(&self) -> bool {
        true
    }

    /// A static a checked body named (fill.rs `body_statics`), as `Front2Program::static_item`.
    fn static_item(&self, key: ItemKey) -> Option<StaticItem> {
        self.statics.kind(key)
    }

    fn foreign_static(&self, key: ItemKey) -> Option<&str> {
        self.statics.foreign(key)
    }

    /// The statics made so far, sorted by key: each one's own data record.
    fn static_records(&self) -> &[(ItemKey, DataRecord<Global>)] {
        &self.statics.values
    }
}

impl<'n, S: NamesSource<'n>> blaze_build::type_name::TypeNames for LazyProgram<'n, S> {
    fn item_path(&self, item: ItemKey, out: &mut dyn FnMut(&str)) -> bool {
        crate::front2::type_name::item_path(self.front.names, &self.front.keys, &self.store, item, out)
    }
}
