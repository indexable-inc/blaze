//! The seam ([`crate::seam::FrontEnd`]) over the lazy program, by method group. A trait's impl
//! is one block, so the groups are its sections, each answering from the columns, the head
//! store, or the live bodies; none reads a table of every body.
//!
//! Groups: (1) rows and bodies: `fn_count`, `function`, `c_variadic`, `entry`, `types`, `sig`,
//! `fn_index`, `code_of`, `intrinsic`, `body_reads`, `check_all`, `check_one`, `done`;
//! (2) the body's world: `resolve`, `selects`, `normalize`, `last_projection`; (3) lang items:
//! `fn_lang`, `drop_trait`, `lang_item`, `lang_named`, `lang_trait_fn`, `fmt_argument_adt`,
//! `fmt_argument_new`; (4) types and items: `adt`, `repr`, `discriminant`, `ctor_of`,
//! `opaque_of`, `object_trait`, `item_named`, `left_out`; (5) literals: `int_lit`, `int_pat`,
//! `string`, `float_lit`, `float_pat`, `str_pat`, `format_template`; (6) constants and statics:
//! `const_record`, `const_at`, `evaluates_at_args`, `static_item`, `static_records`.

use super::*;

use crate::seam::FrontEnd;

impl<'n, S: NamesSource<'n>> LazyProgram<'n, S> {
    /// `f` over the checker's world of row `i`'s live body: as `Front2Program::in_world`. A row
    /// with no live body has no world: the question is the world's own refusal (`Missing`),
    /// naming the fn (the back half asks only in the body it builds, made by `check_one`).
    fn in_world<T>(&self, i: usize, f: impl FnOnce(&BodyWorld<'_, 'n, S>) -> T) -> Result<T, ResolveError> {
        let Some(live) = self.live_of(i) else {
            return Err(ResolveError::Missing(tc::Question::FnSig(self.rows.key[i])));
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
            fills: RefCell::new(Vec::new_in(Global)),
            gaps: RefCell::new(Vec::new_in(Global)),
        };
        Ok(f(&world))
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

    /// The entry's row once `check_one` made it.
    fn entry(&self) -> Option<usize> {
        self.entry_key.and_then(|k| self.rows.by_key.get(&k)).map(|&i| i as usize)
    }

    fn types(&self) -> PTys<'_> {
        self.tys.as_ref().map_or(tc::PTys::EMPTY, |t| t.view())
    }

    fn resolve(&self, i: usize, item: ItemKey, args: List, ground: &mut PortableTys<'_>) -> Result<(ItemKey, List), ResolveError> {
        // As `Front2Program::resolve`. Both helpers read only the solved names; over records
        // `check_one` has already refused every body that calls anything (a callee's row asks
        // `placed`, which refuses by name), so a body with a call never reaches here over them.
        if let Some(names) = self.front.names.solved() {
            if let Some(handler) = crate::front2::panic_entry::bound(names, &self.front.index, &self.store, item, Alloc::global()) {
                return Ok((handler, List::EMPTY));
            }
            if let Some(default) = crate::front2::alloc_shim::bound(names, &self.front.index, &self.store, item, Alloc::global()) {
                return Ok((default, List::EMPTY));
            }
        }
        let r = self.in_world(i, |w| resolve_instance(w, item, args, ground, Alloc::global()))??;
        Ok((r.item, r.args))
    }

    fn selects(&self, i: usize, trait_: ItemKey, ty: PTy, ground: &mut PortableTys<'_>) -> Result<bool, ResolveError> {
        Ok(self.in_world(i, |w| select_impl(w, trait_, &[ty], ground, Alloc::global()))??.is_some())
    }

    /// Every row has its head in the store or is a ctor: `row` makes no row otherwise.
    fn sig(&self, i: usize) -> FnSig<'_> {
        let key = self.rows.key[i];
        let types = self.types();
        if let Some(c) = self.store.ctors.get(&key) {
            let header = tc::FnPtrHeader { abi: tc::Abi::Rust, safety: tc::Safety::Safe };
            return FnSig { parent_count: 0, own_count: c.generics, declared_count: c.generics, self_param: false, header, c_variadic: false, inputs: &c.inputs[..], output: c.output, types };
        }
        let f = &self.store.fns[&key];
        FnSig {
            parent_count: f.parent,
            own_count: f.own,
            declared_count: f.declared,
            self_param: f.self_param,
            header: f.header,
            c_variadic: false,
            inputs: &f.inputs,
            output: f.output,
            types,
        }
    }

    /// Only rows already made: a row is added by `check_one` alone (module docs).
    fn fn_index(&self, key: ItemKey) -> Option<usize> {
        self.rows.by_key.get(&key).map(|&i| i as usize)
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
        let out = self.in_world(i, |w| blaze_typeck::normalize_projection(w, trait_, name, args, ground, Alloc::global()))?;
        if !matches!(out, Ok(Some(_))) {
            let trait_shown = self.item_named(trait_).unwrap_or_else(|| format!("{:#x}", trait_.0).into_boxed_str());
            let why = match &out {
                Err(e) => {
                    let d = format!("{e:?}");
                    String::from(&d[..d.find(['(', ' ', '{']).unwrap_or(d.len())])
                }
                Ok(_) => String::from("the solver's step gave no type"),
            };
            *self.projection_why.borrow_mut() = Some(format!("`{trait_shown}`'s associated type {:#x}: {why}", name.0).into_boxed_str());
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
        match crate::front2::langs::find(&self.front.index, self.front.resolver, item) {
            Ok(found) => Ok(found.map(checker_of)),
            Err(why) => Err(crate::seam::LangRefusal { item, why }),
        }
    }

    fn lang_named(&self, name: &str) -> Option<ItemKey> {
        self.front.index.lang(name).map(checker_of)
    }

    fn lang_trait_fn(&self, trait_: LangItem, name: &str) -> Option<ItemKey> {
        self.front.keys.filed_member(self.made_lang(trait_)?, MemberKind::Fn, name)
    }

    fn fmt_argument_adt(&self) -> Option<ItemKey> {
        self.front.index.lang("format_argument").map(checker_of)
    }

    /// As `Front2Program::fmt_argument_new`. Over records the index files impls only as a
    /// question's bucket admits them, so this reads the solved names alone; over records no
    /// answer, and the builder refuses naming the constructor (listed in the notes).
    fn fmt_argument_new(&self, trait_: tc::FmtTrait) -> Option<ItemKey> {
        self.front.names.solved()?;
        let name = crate::front2::fmt_template::argument_new_name(trait_);
        let adt = self.front.index.lang("format_argument")?;
        let module = self.front.names.home_of(adt)?;
        let mut found = self
            .front
            .index
            .impls
            .iter()
            .filter(|i| i.module == module && crate::front2::fmt_template::inherent_on(i.header, "Argument"))
            .filter_map(|i| self.front.keys.filed_member(checker_of(i.key), MemberKind::Fn, name));
        let first = found.next()?;
        found.next().is_none().then_some(first)
    }

    // (4) Types and items.

    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        let a = self.store.adts.get(&key)?;
        Some(Adt { kind: a.kind, generics: a.generics, variants: &a.variants, fields: &a.fields, types: self.types() })
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

    /// `check_one` refuses a body naming a `dyn` principal ([`LazyRefusal::NotLazyYet`]), so
    /// no build of this program asks.
    fn object_trait(&self, _t: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>> {
        None
    }

    /// `crate::module::Name`, over the source's questions (module path, name).
    fn item_named(&self, key: ItemKey) -> Option<Box<str>> {
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

    fn str_pat(&self, owner: ItemKey, p: PatId) -> Option<&str> {
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
    fn const_at(&mut self, need: crate::front2::const_at::ConstNeed, from: tc::PTys<'_>, args: &[tc::PTy]) -> Result<DataRecord<Global>, crate::front2::const_at::ConstAtWhy> {
        let prepared = self.prepared;
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
            crate::front2::const_at::ConstNeed::Item(item) => self.consts.value_with_args(&mut records, prepared, item, from, args, Alloc::global()),
            crate::front2::const_at::ConstNeed::Block { owner, block } => {
                let Some(text) = records.store.fns.get(&owner).map(|h| h.text) else {
                    return Err(crate::front2::const_at::ConstAtWhy::Eval(crate::front2::consts::ConstFail::Body { item: owner, error: BodyError::NoHead(owner) }));
                };
                let key = crate::front2::const_at::block_key(owner, block, text);
                self.consts.block_value(&mut records, prepared, owner, block, key, from, args, Alloc::global())
            }
        };
        made.map(crate::front2::const_at::data_record).map_err(crate::front2::const_at::ConstAtWhy::Eval)
    }

    fn evaluates_at_args(&self) -> bool {
        true
    }

    /// `check_one` refuses a body naming a static ([`LazyRefusal::NotLazyYet`]).
    fn static_item(&self, _key: ItemKey) -> Option<StaticItem> {
        None
    }

    fn static_records(&self) -> &[(ItemKey, DataRecord<Global>)] {
        &[]
    }
}
