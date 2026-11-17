//! The ONE place rows are added (`&mut self`): `check_one` and what it makes, and `done`.
//!
//! Per body, what `Front2Program::make_from` does for the whole program: the body lowered and
//! checked (`lower_made`, `check_body`), a row for each fn it calls (as `program/tabled.rs`
//! `row` makes one, without a body), the row of each ADT its types name (make_from's rounds),
//! each constant it reads by value (`const_values`). Then the types table forked again with
//! the live bodies (module docs). Each step is bounded by the one body.

use super::*;

use crate::front2::check::eval_len;
use crate::front2::program::head_abi::{BuildAbi, head_abi};
use crate::front2::heads::{HeadError, Want};
use crate::front2::keys::Origin;
use crate::front2::lower_made::{LowerTarget, lower_made};

impl<'n, S: NamesSource<'n>> LazyProgram<'n, S> {
    /// The seam's `check_one` (module docs, step 1). `Ok(None)` is never answered: a key this
    /// program cannot find (lazy/locate.rs searched its crate's records) is
    /// [`LazyRefusal::NotFound`], naming what was searched.
    pub(super) fn check_one_key(&mut self, key: ItemKey) -> Result<Option<(usize, Checked<'static>)>, LazyRefusal> {
        if let Some(&i) = self.rows.by_key.get(&key)
            && self.rows.checked[i as usize]
        {
            return Err(LazyRefusal::AskedTwice { key });
        }
        let (target, _) = self.target_of(key)?;
        let function: Box<str> = self.shown(key).into_boxed_str();
        let prepared = self.prepared;
        let made = {
            let mut r = self.records(true);
            lower_made(&mut r, prepared, target)
        }
        .map_err(|error| LazyRefusal::Lower { function: function.clone(), error })?;
        let checked = {
            let view = made.lowered.view();
            let spell = Spelling::of(&view).map_err(|_| LazyRefusal::Spell { function: function.clone() })?;
            let scope = made.scope(&spell);
            let mut r = self.records(true);
            check_body(&mut r, &made, &scope).map_err(|error| LazyRefusal::Check { function: function.clone(), error })?
        };
        self.counts.checked += 1;
        let i = self.row(key, false)?;
        self.rows.checked[i] = true;
        // Each fn the body calls: its head row (the build reads a call's ABI and signature
        // from the item the call names). A trait's fn called by its trait's path is tabled as
        // declared, as make_from tables it.
        for &callee in checked.callees.column::<1>() {
            let of_trait = matches!(self.front.keys.origin(callee), Some(Origin::Member { container, kind: MemberKind::Fn, .. }) if !self.store.impls.contains_key(container));
            self.row(callee, of_trait)?;
        }
        self.async_rows(&made)?;
        self.adt_rows(&checked)?;
        self.body_consts(&checked)?;
        self.body_statics(&checked, &function)?;
        self.body_objects(&checked, &function)?;
        // Its async blocks' state recipes (coroutine_recipes.rs, as `Front2Program::make_from`
        // makes them), AFTER its callees' heads are in the store: the lowering relates a call
        // through its callee's signature (an `async fn` called with an `.await`, its future's
        // lifetimes), and with no head there the call is refused `ElisionAmbiguous`. A body
        // with no async block asks nothing.
        self.body_recipes(&made, &checked, &function)?;
        self.refork(i, made, &function)?;
        Ok(Some((i, checked)))
    }

    /// Decides where the process starts, once, by THE start rule (program.rs `start_of`, as
    /// run/special.rs `roots` takes it): an exported entry; else a Rust root's `fn main` (found
    /// by run_entry.rs `root_main`) through the linked `#[lang = "start"]`, its head read into
    /// THIS program's store (start.rs `rust_start`: `T`, `main`'s return type, is a type of the
    /// store's table, the one the back half reads); else `main` called directly. What the seam's
    /// `rust_start` answers. Reads crate 0's fn rows and their attributes, the linked crates'
    /// lang rows, and for a Rust start `main`'s head; no body.
    pub fn decide_start(&mut self) -> Result<(), crate::run::special::RootsError> {
        use crate::front2::start::{Entry2, rust_start};
        use crate::run::special::RootsError;
        let names = self.front.names;
        let entry = match start_of(names, self.alloc).map_err(RootsError::Start)? {
            StartOf::Exported(e) => e,
            StartOf::LangStart(lang_start) => {
                let main = crate::front2::program::run_entry::root_main(names).map_err(RootsError::Entry)?;
                Entry2::Rust(rust_start(names, lang_start, main, &mut self.store, &mut self.front.keys).map_err(RootsError::Start)?)
            }
            StartOf::Direct => Entry2::Direct(crate::front2::program::run_entry::root_main(names).map_err(RootsError::Entry)?),
        };
        self.start = Some(entry);
        Ok(())
    }

    /// The seam's `ctor_row`: the ctor `key`'s row, made once by [`Self::row`] (a ctor's arm:
    /// no body, no `extern` block asked). `Ok(None)` for a key that is no ctor of a made ADT
    /// head (the store's `ctors`): the head of the ADT a body names is made when that body is
    /// checked (`adt_rows`), so a ctor the walk reaches is filed by then.
    pub(super) fn ctor_row_key(&mut self, key: ItemKey) -> Result<Option<usize>, LazyRefusal> {
        if !self.store.ctors.contains_key(&key) {
            return Ok(None);
        }
        self.row(key, false).map(Some)
    }

    /// The seam's `done`: row `i`'s body is dropped (its head facts stay).
    pub(super) fn done_row(&mut self, i: usize) {
        let Some(slot) = self.rows.live.get_mut(i).and_then(Option::take) else { return };
        if let Some(cell) = self.live.get_mut(slot as usize) {
            *cell = None;
        }
        self.counts.live = self.live.iter().filter(|l| l.is_some()).count() as u32;
    }

    /// How fn `key`'s body is lowered and its head made.
    /// A key the key map does not know yet is first found and filed from the records
    /// (lazy/locate.rs: an item row, a member of a container whose head is not made, an item
    /// declared in a body); the container's head is then the want, made on demand.
    fn target_of(&mut self, key: ItemKey) -> Result<(LowerTarget, Want), LazyRefusal> {
        self.locate(key)?;
        if let Some(lib) = self.front.keys.library_in(self.front.names, key) {
            return Ok((LowerTarget::Free(lib), self.store.need(lib)));
        }
        match self.front.keys.origin(key) {
            Some(Origin::Member { container, kind: MemberKind::Fn, .. }) => {
                let container = *container;
                let Some(c) = self.front.keys.library_in(self.front.names, container) else {
                    return Err(LazyRefusal::NotFound { key, krate: self.refused_shown(key), searched: format!("its container {:032x} is filed with no library key", container.0).into_boxed_str() });
                };
                let want = if self.store.impls.contains_key(&container) { Want::MemberFn(c, key) } else { Want::Members(c) };
                Ok((LowerTarget::Member(key), want))
            }
            Some(Origin::Member { kind, .. }) => Err(LazyRefusal::NotFound { key, krate: self.refused_shown(key), searched: format!("it is a member of kind {kind:?}, not a fn with a body").into_boxed_str() }),
            Some(Origin::Item(_)) | None => Err(LazyRefusal::NotFound { key, krate: self.refused_shown(key), searched: Box::from("the key map filed it with no library key") }),
        }
    }

    /// `want` made, each constant its head names evaluated as met (at most `MOST_ADT_LENS`),
    /// as make_from's ADT rounds do.
    fn make_want(&mut self, want: Want, item: ItemKey) -> Result<(), LazyRefusal> {
        let mut lens = 0;
        loop {
            let names = self.front.names;
            match self.store.ensure(names, &mut self.front.keys, want) {
                Err(HeadError::Item(_, Why::NeedConst(c, _))) if lens < MOST_ADT_LENS => {
                    lens += 1;
                    let mut r = self.records(true);
                    if let Err(e) = eval_len(&mut r, c) {
                        return Err(LazyRefusal::Head { item: self.refused_shown(item), why: format!("the constant {c:x} its head names: {e:?}").into_boxed_str() });
                    }
                }
                Err(e) => return Err(LazyRefusal::Head { item: self.refused_shown(item), why: crate::front2::item_shown::head_error_shown(names, &e).into_boxed_str() }),
                Ok(()) => return Ok(()),
            }
        }
    }

    /// Fn `key`'s row, made once: its head facts as `program/tabled.rs` `row` reads them, with
    /// no body (a body is `check_one`'s). Returns its index.
    fn row(&mut self, key: ItemKey, of_trait: bool) -> Result<usize, LazyRefusal> {
        if let Some(&i) = self.rows.by_key.get(&key) {
            return Ok(i as usize);
        }
        let row_path = crate::front2::row_path::RowPath::of(self.front.names, &self.front.keys, &self.store, key)
            .map_err(|no| LazyRefusal::Row { function: no.item.boxed(), refusal: Box::new(Front2Refusal::NoRowPath(no)) })?;
        // `path` is for messages; the item's identifier, data, is `row_path.ident()` (rule 30).
        let path = String::from(row_path.text());
        let refuse = |refusal: Front2Refusal| LazyRefusal::Row { function: Box::from(path.as_str()), refusal: Box::new(refusal) };
        // A ctor is the language's: tabled as one; the build makes its value.
        if self.store.ctors.contains_key(&key) {
            // Placed by the one rule (head_abi.rs `placed_ctor`): no `extern` block is asked.
            let at = crate::front2::program::head_abi::placed_ctor(self.front.names, self.front.keys.library_in(self.front.names, key));
            // A ctor is a safe Rust fn (head_abi.rs `head_abi`, the one home): unwinding, never
            // nounwind, never tracked.
            let ctor = head_abi(self.front.names, &self.store, key, || Box::from(path.as_str())).map_err(&refuse)?;
            return Ok(self.push(key, FnKind::Ctor, ctor, false, at, &path, "", None, None));
        }
        if !self.store.fns.contains_key(&key) {
            let (_, want) = self.target_of(key)?;
            self.make_want(want, key)?;
        }
        let Some(head) = self.store.fns.get(&key) else {
            return Err(refuse(Front2Refusal::NoHead { function: Box::from(path.as_str()) }));
        };
        let (lib, text, in_container) = (head.lib, head.text, head.container.is_some());
        let symbols = symbols_of(self.front.names, lib, text, &path).map_err(&refuse)?;
        let (at, foreign) = self.placed_of(key, lib, &path)?;
        let local = |why: crate::front2::panic_entry::Refused| Front2Refusal::Attrs { function: Box::from(path.as_str()), why: why.form().into() };
        let foreign = foreign.or_else(|| crate::front2::panic_entry::local_abi(&self.store, key));
        let declared_name = crate::front2::panic_entry::local_name(&self.store, key).map_err(local).map_err(&refuse)?;
        let alloc = self.store.tys.alloc();
        let panic_entry = crate::front2::panic_entry::declares(self.front.names, &self.store, key, alloc).map_err(local).map_err(&refuse)?;
        let alloc_shim = crate::front2::alloc_shim::declares(self.front.names, &self.store, key, alloc)
            .map_err(|why| refuse(Front2Refusal::Attrs { function: Box::from(path.as_str()), why: why.detail() }))?;
        // As tabled.rs `row`, less its `NoBody` arm: a fn with a body is `Free` here whether or
        // not `check_one` made the body yet (the seam's `function(i).body` is `Some` once it has).
        let (kind, symbol) = if panic_entry || alloc_shim.is_some() {
            (FnKind::Free, Box::<str>::from(""))
        } else if foreign.is_some() && !in_container {
            let symbol = symbols.link_name.clone().unwrap_or_else(|| declared_name.unwrap_or(row_path.ident()).into());
            (FnKind::Foreign, symbol)
        } else if of_trait {
            (FnKind::Trait(0), Box::<str>::from(""))
        } else {
            (FnKind::Free, Box::<str>::from(""))
        };
        let intrinsic = if symbols.intrinsic {
            let name = row_path.ident();
            match blaze_build::intrinsics::Which::named(name) {
                Ok(which) => Some(Intrinsic::Table(which)),
                Err(_) => return Err(refuse(Front2Refusal::Intrinsic { function: Box::from(path.as_str()) })),
            }
        } else if alloc_shim == Some(crate::front2::alloc_shim::Shim::Marker) {
            match blaze_build::intrinsics::Which::named(crate::front2::alloc_shim::Shim::Marker.declared()) {
                Ok(which) => Some(Intrinsic::Table(which)),
                Err(_) => return Err(refuse(Front2Refusal::Intrinsic { function: Box::from(path.as_str()) })),
            }
        } else {
            None
        };
        // The ABI and its unwind category, from the ONE home of the rule (program/head_abi.rs
        // `head_abi`, which tabled.rs `row` calls too): a fn's own head ("C" nounwind,
        // "C-unwind" not), or a foreign fn's block. An ABI with no reading is refused by name.
        let own = head_abi(self.front.names, &self.store, key, || Box::from(path.as_str())).map_err(&refuse)?;
        // `#[rustc_nounwind]` makes any fn nounwind (core/src/panicking.rs:88-92), as tabled.rs.
        let build = BuildAbi { abi: own.abi, nounwind: own.nounwind || symbols.nounwind };
        Ok(self.push(key, kind, build, symbols.track_caller, at, &path, &symbol, symbols.export.as_deref(), intrinsic))
    }

    /// Fn `key`'s own `#[track_caller]` (program/seam_rules.rs `track_caller` asks it of the fn
    /// and of its trait's declaration): its row's when it has one, else read from its head in
    /// the store (`symbols_of`, as `row` reads it) without adding a row (`&self`: a callee the
    /// build resolved to, whose row its own `check_one` makes later). `None`: no row and no
    /// head, or the head's attributes refused.
    pub(super) fn own_track_caller(&self, key: ItemKey) -> Option<bool> {
        if let Some(&i) = self.rows.by_key.get(&key) {
            return self.rows.track_caller.get(i as usize).copied();
        }
        if self.store.ctors.contains_key(&key) {
            return Some(false);
        }
        let head = self.store.fns.get(&key)?;
        symbols_of(self.front.names, head.lib, head.text, "").ok().map(|s| s.track_caller)
    }

    #[allow(clippy::too_many_arguments)]
    fn push(&mut self, key: ItemKey, kind: FnKind, build: BuildAbi, track_caller: bool, at: At, path: &str, symbol: &str, export: Option<&str>, intrinsic: Option<Intrinsic>) -> usize {
        let i = self.rows.len();
        let path = self.rows.put(path);
        let symbol = self.rows.put(symbol);
        let export = export.map(|e| self.rows.put(e));
        self.rows.key.push(key);
        self.rows.kind.push(kind);
        self.rows.abi.push(build.abi);
        self.rows.nounwind.push(build.nounwind);
        self.rows.track_caller.push(track_caller);
        self.rows.at.push(at);
        self.rows.path.push(path);
        self.rows.symbol.push(symbol);
        self.rows.export.push(export);
        self.rows.intrinsic.push(intrinsic);
        self.rows.live.push(None);
        self.rows.checked.push(false);
        self.rows.by_key.insert(key, i as u32);
        self.counts.rows += 1;
        i
    }

    /// `placed` over this program's names source; an unanswered `extern_abi` is refused by
    /// name (never taken for "not foreign").
    fn placed_of(&self, key: ItemKey, lib: LibKey, item: &str) -> Result<(At, Option<&'n str>), LazyRefusal> {
        crate::front2::program::head_abi::placed_fn(self.front.names, &self.store, key, lib).map_err(|_| LazyRefusal::Unanswered { question: "extern_abi", item: Box::from(item) })
    }

    /// A fn's name for messages (front2 row_path.rs `shown_name`, the one printer): its row
    /// path, else its `ItemShown`. A row's path is `row`'s `RowPath`, never this text.
    pub(super) fn shown(&self, k: ItemKey) -> String {
        crate::front2::row_path::shown_name(self.front.names, &self.front.keys, &self.store, k)
    }

    /// `k` for a refusal (rule 19): its whole path and its key (front2 item_shown.rs
    /// `ItemShown`); [`Self::shown`] stays the bare name a row's path is made from.
    pub(super) fn refused_shown(&self, k: ItemKey) -> Box<str> {
        crate::front2::item_shown::ItemShown::of(self.front.names, &self.front.keys, k).boxed()
    }

    /// The row of every ADT `checked`'s types and the heads' table name, in rounds as a row's
    /// fields name more (make_from's loop, over ONE body). A narrowed valid range is refused as
    /// make_from refuses it.
    fn adt_rows(&mut self, checked: &Checked<'static>) -> Result<(), LazyRefusal> {
        for round in 0.. {
            let mut missing: AVec<'_, ItemKey> = AVec::new_in(self.alloc);
            for v in [self.store.tys.view(), checked.types.view()] {
                for n in v.nodes() {
                    if let tc::PNode::Adt(k, _) = n
                        && !self.store.adts.contains_key(k)
                    {
                        missing.push(*k);
                    }
                }
            }
            missing.sort_unstable();
            missing.dedup();
            let Some(&first) = missing.first() else { break };
            if round == super::super::super::reach::MOST_FNS {
                return Err(LazyRefusal::Adt { adt: self.refused_shown(first), why: Box::from("rows still missing after the most rounds") });
            }
            for &k in missing.iter() {
                // `library_in`: over records the keys map files no item (keys.rs).
                let Some(lib) = self.front.keys.library_in(self.front.names, k) else {
                    return Err(LazyRefusal::Adt { adt: self.refused_shown(k), why: Box::from("no library key") });
                };
                self.adt_row(lib, k)?;
            }
        }
        if let Some(k) = self.store.adts.iter().filter(|(_, a)| a.narrowed).map(|(k, _)| *k).min() {
            let adt = self.refused_shown(k);
            return Err(LazyRefusal::Row { function: adt.clone(), refusal: Box::new(Front2Refusal::Layout { adt }) });
        }
        Ok(())
    }

    /// The ADT `k` (library item `lib`) made and filed under its key, or refused by name.
    fn adt_row(&mut self, lib: LibKey, k: ItemKey) -> Result<(), LazyRefusal> {
        self.make_want(Want::Adt(lib), k).map_err(|e| match e {
            LazyRefusal::Head { item, why } => LazyRefusal::Adt { adt: item, why },
            other => other,
        })?;
        if !self.store.adts.contains_key(&k) {
            return Err(LazyRefusal::Adt { adt: self.refused_shown(k), why: Box::from("made, but no row filed under its key") });
        }
        Ok(())
    }

    /// For a body with an async block, the rows of the library's async items the build of ANY
    /// coroutine reads: the fns an `.await` calls (below), and the ADTs (front2/coroutines.rs `async_items`: `Poll`'s variants by
    /// name, `Pin`), as the whole program has them from its census: the body's own types need
    /// not name them (`add_slowly` names no `Poll`; the executor that matches on it may be
    /// checked later or never). A library without such an item files nothing here: the
    /// builder refuses the async block naming `Adts::async_items`. Rows their fields name are
    /// made by `adt_rows` after this.
    fn async_rows(&mut self, made: &crate::front2::body::Body<'_>) -> Result<(), LazyRefusal> {
        if !made.typeck.exprs.iter().any(|e| matches!(e, tc::Expr::AsyncBlock { .. })) {
            return Ok(());
        }
        for name in ["Poll", "pin"] {
            let Some(lib) = self.front.index.lang(name) else { continue };
            let k = crate::front2::keys::checker_of(lib);
            if !self.store.adts.contains_key(&k) {
                self.adt_row(lib, k)?;
            }
        }
        // The trait fns an `.await` and a coroutine's entries call, which the body's callees do
        // not list (the builder makes those calls, blaze-build coroutine.rs, by the keys the
        // seam's `lang_named` gives): each one's head made, so the build reads its ABI from it
        // (`fn_abi`: no row, the head's). A head not filed under that key is refused by name.
        for name in ["into_future", "poll", "coroutine_resume"] {
            let Some(lib) = self.front.index.lang(name) else { continue };
            // The key the seam's `lang_named` and `lang_item` give (keys.rs `asked_key`): a
            // trait's fn is filed under its member key, not its library key's first bytes.
            let k = crate::front2::keys::asked_key(self.front.names, lib);
            if self.store.fns.contains_key(&k) {
                continue;
            }
            // A trait's fn is a library-keyed member (by_origin.rs `kind_of_lib`): its head is
            // made with its trait's members (`Store::need` of the owner).
            let want = match self.front.names.member_owner(lib) {
                Some((owner, _)) => self.store.need(owner),
                None => self.store.need(lib),
            };
            // No head filed under `k` after this is refused where it is needed, by name (the
            // build's `NoAbi`: "the build's `Adts` gave no ABI for it").
            self.make_want(want, k)?;
        }
        // Each one's row, as a callee's: the build resolves the call it makes by the fn's row
        // (`fn_index`), a trait's fn tabled as declared, as `check_one_key` tables a callee.
        for name in ["into_future", "poll", "coroutine_resume"] {
            let Some(lib) = self.front.index.lang(name) else { continue };
            let k = crate::front2::keys::asked_key(self.front.names, lib);
            if !self.store.fns.contains_key(&k) {
                continue;
            }
            let of_trait = matches!(self.front.keys.origin(k), Some(Origin::Member { container, kind: MemberKind::Fn, .. }) if !self.store.impls.contains_key(container));
            self.row(k, of_trait)?;
        }
        Ok(())
    }

    /// Each constant `checked` reads by value, evaluated once ([`const_values`], the one home),
    /// filed in key order.
    fn body_consts(&mut self, checked: &Checked<'static>) -> Result<(), LazyRefusal> {
        let (values, refused) = {
            let mut r = self.records(true);
            const_values(&mut r, core::iter::once(checked))
        };
        if let Some((k, why)) = refused.into_iter().next() {
            return Err(LazyRefusal::Const { constant: self.refused_shown(k), why });
        }
        for (k, record) in values {
            if let Err(at) = self.const_values.binary_search_by_key(&k, |(x, _)| *x) {
                self.const_values.insert(at, (k, record));
            }
        }
        Ok(())
    }

    /// Each static `checked` names that this program has not made, with each static their
    /// records point at, evaluated once (statics.rs `static_values`, the one home) and merged
    /// into the sorted `statics`. A refused static stops the run, naming it (as `make_from`'s
    /// run does: never an empty answer to the getter).
    fn body_statics(&mut self, checked: &Checked<'static>, function: &str) -> Result<(), LazyRefusal> {
        let new = checked.expr_defs.column::<1>().iter().any(|d| match d {
            tc::Def::Static(k, _) => self.statics.kind(*k).is_none(),
            _ => false,
        });
        if !new {
            return Ok(());
        }
        let (made, refused) = {
            let mut r = self.records(true);
            crate::front2::statics::static_values(&mut r, core::iter::once(checked))
        };
        if let Some((k, why)) = refused.into_iter().next() {
            let static_ = self.refused_shown(k);
            return Err(LazyRefusal::Row { function: Box::from(function), refusal: Box::new(Front2Refusal::Static { static_, why }) });
        }
        let crate::front2::statics::Statics { values, kinds } = made;
        for (k, record) in values {
            if let Err(at) = self.statics.values.binary_search_by_key(&k, |(x, _)| *x) {
                self.statics.values.insert(at, (k, record));
            }
        }
        for (k, kind) in kinds {
            if let Err(at) = self.statics.kinds.binary_search_by_key(&k, |(x, _)| *x) {
                self.statics.kinds.insert(at, (k, kind));
            }
        }
        Ok(())
    }

    /// Static `item` as a refusal names it (its path, or its key).
    pub fn static_shown(&self, item: ItemKey) -> Box<str> {
        self.refused_shown(item)
    }

    /// Static `item`'s value and kind, for its run record (code_records.rs `statics`): the one
    /// this program made when a body named it, or evaluated now, alone (statics.rs
    /// `one_static`; the statics its value points at are the walk's own rows, each asked by
    /// its record, never here). A refusal names the static.
    pub fn static_value(&mut self, item: ItemKey) -> Result<(&blaze_build::data::DataRecord<Global>, blaze_build::StaticItem), LazyRefusal> {
        if self.statics.kind(item).is_none() {
            let made = {
                let mut r = self.records(true);
                crate::front2::statics::one_static(&mut r, item)
            };
            let (record, kind) = made.map_err(|why| {
                let static_ = self.refused_shown(item);
                LazyRefusal::Row { function: static_.clone(), refusal: Box::new(Front2Refusal::Static { static_, why }) }
            })?;
            if let Err(at) = self.statics.values.binary_search_by_key(&item, |(x, _)| *x) {
                self.statics.values.insert(at, (item, record));
            }
            if let Err(at) = self.statics.kinds.binary_search_by_key(&item, |(x, _)| *x) {
                self.statics.kinds.insert(at, (item, kind));
            }
        }
        let kind = self.statics.kind(item).ok_or(LazyRefusal::NoStatic { item })?;
        let at = self.statics.values.binary_search_by_key(&item, |(x, _)| *x).map_err(|_| LazyRefusal::NoStatic { item })?;
        Ok((&self.statics.values[at].1, kind))
    }

    /// The object table of each `dyn` principal `checked`'s types or the heads' table name
    /// (what `make_from` asks over every body: here over this one) that this program has not
    /// made, with every supertrait under it (program.rs `object_tables`, the one home). A
    /// refused trait stops the run, naming it.
    /// The state recipe of each async block of `made`, filed in (owner, ordinal) order for the
    /// seam's `coroutine_recipe` (coroutine_recipes.rs `recipes_of` and `find`, the one home).
    /// A block whose flow is refused is filed with its refusal, as `make_from` files it.
    fn body_recipes(&mut self, made: &crate::front2::body::Body<'_>, checked: &Checked<'static>, function: &str) -> Result<(), LazyRefusal> {
        let mut recipes: Vec<crate::front2::coroutine_recipes::RecipeRow, Global> = Vec::new_in(Global);
        let alloc = self.alloc;
        {
            let mut r = self.records(true);
            crate::front2::coroutine_recipes::recipes_of(&mut r, made, checked, &mut recipes, alloc)
        }
        .map_err(|error| LazyRefusal::Check { function: Box::from(function), error: CheckFail::Check { owner: made.typeck.owner, error } })?;
        for row in recipes {
            if let Err(at) = self.recipes.binary_search_by_key(&(row.0, row.1), |r| (r.0, r.1)) {
                self.recipes.insert(at, row);
            }
        }
        Ok(())
    }

    fn body_objects(&mut self, checked: &Checked<'static>, function: &str) -> Result<(), LazyRefusal> {
        let mut asked = dyn_principals([self.store.tys.view(), checked.types.view()].into_iter());
        asked.retain(|k| !self.objects.contains_key(k));
        if asked.is_empty() {
            return Ok(());
        }
        let mut objects = core::mem::replace(&mut self.objects, HashMap::new_in(Global));
        let made = {
            let mut r = self.records(true);
            object_tables(&mut r, asked, &mut objects, None)
        };
        self.objects = objects;
        made.map_err(|refusal| LazyRefusal::Row { function: Box::from(function), refusal: Box::new(refusal) })
    }

    /// The types table forked afresh from the heads' and each live body lowered again into it,
    /// then `made` (row `i`'s new body) too, its literals read: module docs.
    fn refork(&mut self, i: usize, made: Body<'n>, function: &str) -> Result<(), LazyRefusal> {
        let lower = |error| LazyRefusal::Lower { function: Box::from(function), error };
        let mut tys = self.store.tys.fork_in(self.store.tys.alloc()).map_err(|_| LazyRefusal::Table { function: Box::from(function) })?;
        let (names, prepared) = (self.front.names, self.prepared);
        for l in self.live.iter_mut().flatten() {
            l.again = lower_again(names, prepared, &mut self.front.keys, &mut self.store, &l.made, &mut tys).map_err(lower)?;
        }
        let again = lower_again(names, prepared, &mut self.front.keys, &mut self.store, &made, &mut tys).map_err(lower)?;
        self.tys = Some(tys);
        let lits = literals(&again, function).map_err(|refusal| LazyRefusal::Row { function: Box::from(function), refusal: Box::new(refusal) })?;
        let live = Live { row: i as u32, made, again, lits };
        let slot = match self.live.iter().position(Option::is_none) {
            Some(free) => {
                self.live[free] = Some(live);
                free
            }
            None => {
                self.live.push(Some(live));
                self.live.len() - 1
            }
        };
        self.rows.live[i] = Some(slot as u32);
        self.counts.live = self.live.iter().filter(|l| l.is_some()).count() as u32;
        Ok(())
    }
}
