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

impl<'n, S: NamesSource<'n>> LazyProgram<'n, S> {
    /// `f` over the checker's world of row `i`'s live body: as `Front2Program::in_world`. A ctor
    /// (a row with no body, built as a fn) is asked in its ADT's world (`ctor_world`); any other
    /// row with no live body has no world: the question is the world's own refusal (`Missing`),
    /// naming the fn (the back half asks only in the body it builds, made by `check_one`).
    fn in_world<T>(&self, i: usize, f: impl FnOnce(&BodyWorld<'_, 'n, S>) -> T) -> Result<T, ResolveError> {
        let Some(live) = self.live_of(i) else {
            let key = self.rows.key[i];
            return crate::front2::world_items::ctor_world(self.front.names, &self.front.keys, &self.front.index, &self.store, self.buckets.as_ref(), key, f);
        };
        let body = &live.again;
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
            fills: RefCell::new(Vec::new_in(Global)),
            gaps: RefCell::new(Vec::new_in(Global)),
            // As `Front2Program::in_world`: every question is at ground arguments after the
            // body checked (rustc's post-analysis mode), so every opaque is its hidden type.
            reveal: crate::front2::opaque_auto::Reveal::All,
        };
        let out = f(&world);
        // What the question found unmade, for the build's `make_asked`.
        self.asked.borrow_mut().extend(world.fills.into_inner());
        Ok(out)
    }

    fn lits_of(&self, owner: ItemKey) -> Option<&Literals> {
        self.live_by_key(owner).map(|l| &l.lits)
    }
}

impl<'n, S: NamesSource<'n>> FrontEnd for LazyProgram<'n, S> {
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
            body: self.live_of(i).map(|l| (&l.again.typeck, &l.lits.texts)),
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
        self.tys.as_ref().map_or(tc::PTys::EMPTY, |t| t.view())
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
        let fills = core::mem::replace(self.asked.get_mut(), Vec::new_in(Global));
        if fills.is_empty() {
            return Ok(false);
        }
        let owner = self.rows.key[i];
        let function: Box<str> = Box::from(self.rows.str(self.rows.path[i]));
        let mut records = Records {
            names: self.front.names,
            keys: &mut self.front.keys,
            index: &self.front.index,
            store: &mut self.store,
            buckets: &mut self.buckets,
            order: BuildOrder::AsAsked,
            consts: None,
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
            output: f.output,
            types,
        }
    }

    /// From the fn's row (fill.rs `row`): its written ABI's unwind category (program/head_abi.rs
    /// `BuildAbi`: "C"/"system" nounwind, "Rust"/"rust-call"/"-unwind" not) or
    /// `#[rustc_nounwind]`, as `Front2Program`'s. `None`: no row made for `key` yet.
    fn nounwind(&self, key: ItemKey) -> Option<bool> {
        let &i = self.rows.by_key.get(&key)?;
        self.rows.nounwind.get(i as usize).copied()
    }

    /// A row's ABI, else the head's (program/seam_rules.rs `fn_abi`, `Front2Program`'s rule): a
    /// callee the build resolved to before its own `check_one` made its row still has its
    /// signature's ABI.
    fn fn_abi(&self, key: ItemKey) -> Option<Abi> {
        let tabled = self.rows.by_key.get(&key).and_then(|&i| self.rows.abi.get(i as usize).copied());
        super::seam_rules::fn_abi(tabled, self.front.names, &self.store, key)
    }

    /// program/seam_rules.rs `track_caller` over the rows' own attributes, else the heads'
    /// (fill.rs `own_track_caller`): a trait impl's method that does not say it itself answers
    /// with its trait's declaration's.
    fn track_caller(&self, key: ItemKey) -> Option<bool> {
        super::seam_rules::track_caller(&self.front.keys, &self.store, key, |k| self.own_track_caller(k))
    }

    /// From `owner`'s live body (the build asks while it builds that body) by program/
    /// seam_rules.rs `caller_site`: item-relative, the file by its shown path from the names
    /// source (over records, the items record's file row).
    fn caller_site(&self, owner: ItemKey, expr: ExprId) -> Option<blaze_build::CallerSite<'_>> {
        let live = self.live_by_key(owner)?;
        super::seam_rules::caller_site(self.front.names, &self.front.keys, owner, &live.again, expr)
    }

    /// program/seam_rules.rs `source_position`: the item's first byte today and today's text of
    /// its file, from the names source (over records, item_grain.rs `row_now` and `file_now`).
    fn source_position(&self, item: ItemKey, site: u32) -> Result<(u32, u32), crate::seam::PositionGap> {
        super::seam_rules::source_position(self.front.names, &self.front.keys, item, site)
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

    /// Over the source's questions: compiler_builtins' item rows, then the signatures of the
    /// fns named a routine's symbol (front2/builtins_shim.rs).
    fn soft_float_routines(&self, alloc: blaze_own::Alloc<'_>) -> crate::front2::builtins_shim::Builtins {
        crate::front2::builtins_shim::find(self.front.names, alloc)
    }

    fn body_reads(&self, i: usize, _checked: &Checked<'static>) -> Option<crate::front2::body_reads::ReadsDigest> {
        self.store.reads.get(self.rows.key[i])
    }

    /// Never: the lazy program checks one body at a time (`check_one`).
    fn check_all(&mut self) -> Result<Vec<Option<Checked<'static>>, Global>, crate::run::RunError> {
        Err(crate::run::RunError::Front2(Front2Refusal::Lazy(LazyRefusal::NotLazyYet { method: "check_all", item: Box::from("the whole program") })))
    }

    fn check_one(&mut self, key: ItemKey) -> Result<Option<(usize, Checked<'static>)>, crate::run::RunError> {
        self.check_one_key(key).map_err(|e| crate::run::RunError::Front2(Front2Refusal::Lazy(e)))
    }

    fn ctor_row(&mut self, key: ItemKey) -> Result<Option<usize>, crate::run::RunError> {
        self.ctor_row_key(key).map_err(|e| crate::run::RunError::Front2(Front2Refusal::Lazy(e)))
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
    fn normalize(&self, trait_: ItemKey, name: tc::Name, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<Option<PTy>, ResolveError> {
        let Some(i) = self.live.iter().flatten().map(|l| l.row as usize).min() else {
            *self.projection_why.borrow_mut() = Some(Box::from("the lazy program holds no live body to ask in"));
            return Ok(None);
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

    fn lang_trait_fn(&self, trait_: LangItem, name: &str) -> Option<ItemKey> {
        self.front.keys.filed_member(self.made_lang(trait_)?, MemberKind::Fn, name)
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
        use crate::front2::by_origin::{ItemKind, KeyKind, kind_of_key};
        use crate::front2::heads::Want;
        use crate::front2::world_items::Fill;
        let Some(a) = self.store.adts.get(&key) else {
            if let Ok(KeyKind::Item(ItemKind::Adt(lib))) = kind_of_key(self.front.names, &self.front.keys, key) {
                let mut asked = self.asked.borrow_mut();
                if !asked.iter().any(|f| matches!(f, Fill::Store(Want::Adt(l)) if *l == lib)) {
                    asked.push(Fill::Store(Want::Adt(lib)));
                }
            }
            return None;
        };
        // The head store's own table, which holds every head's types: `types()` is a fork of it
        // taken at the last `check_one`, which lacks a row made after (by `make_asked`), and
        // agrees with it on every row made before. Every reader grounds a field's type from
        // `Adt::types` alone (blaze-build `ground`).
        Some(Adt { kind: a.kind, generics: a.generics, variants: &a.variants, fields: &a.fields, types: self.store.tys.view() })
    }

    fn repr(&self, key: ItemKey) -> ReprAttrs {
        self.store.adts.get(&key).map_or(ReprAttrs::RUST, |a| a.repr)
    }

    fn discriminant(&self, adt: ItemKey, variant: u32) -> Option<i128> {
        self.store.adts.get(&adt)?.discrs.get(variant as usize).copied()
    }

    fn ctor_of(&self, key: ItemKey) -> Option<(ItemKey, u32)> {
        let adt = self.store.ctors.get(&key)?.adt;
        let variant = self.store.adts.get(&adt)?.variants.iter().position(|v| v.key == key)?;
        Some((adt, variant as u32))
    }

    fn opaque_of(&self, opaque: ItemKey) -> Option<(ItemKey, u32)> {
        self.store.opaques.get(&opaque).copied()
    }

    /// The table `check_one` made for a principal the checked bodies name (fill.rs
    /// `body_objects`), as `Front2Program::object_trait`.
    fn object_trait(&self, t: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>> {
        let o = self.objects.get(&t)?;
        Some(blaze_build::vtable::ObjectTrait { supers: &o.supers, super_args: &o.super_args, methods: &o.methods, shape: o.shape })
    }

    /// `crate::module::Name`, over the source's questions (module path, name).
    fn item_named(&self, key: ItemKey) -> Option<Box<str>> {
        // A member (a trait's fn: what a resolve refusal names) by its container's path.
        if let Some(crate::front2::keys::Origin::Member { container, name, .. }) = self.front.keys.origin(key) {
            let container = self.item_named(*container)?;
            return Some(format!("{container}::{name}").into_boxed_str());
        }
        let names = self.front.names;
        let lib = self.front.keys.library_in(names, key)?;
        let name = names.name_of(lib)?;
        let module = names.module_path(names.home_of(lib)?)?;
        Some(format!("{module}::{name}").into_boxed_str())
    }

    /// A run leaves nothing out: a fn that does not check is `check_one`'s refusal.
    fn left_out(&self, _key: ItemKey) -> Option<&str> {
        None
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

    fn evaluates_at_args(&self) -> bool {
        true
    }

    /// A static a checked body named (fill.rs `body_statics`), as `Front2Program::static_item`.
    fn static_item(&self, key: ItemKey) -> Option<StaticItem> {
        self.statics.kind(key)
    }

    /// The statics made so far, sorted by key: each one's own data record.
    fn static_records(&self) -> &[(ItemKey, DataRecord<Global>)] {
        &self.statics.values
    }
}
