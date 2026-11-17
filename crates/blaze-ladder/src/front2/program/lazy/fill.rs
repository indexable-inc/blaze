//! The ONE place rows are added (`&mut self`): `check_one` and what it makes, and `done`.
//!
//! Per body, what `Front2Program::make_from` does for the whole program: the body lowered and
//! checked (`lower_made`, `check_body`), a row for each fn it calls (as `program/tabled.rs`
//! `row` makes one, without a body), the row of each ADT its types name (make_from's rounds),
//! each constant it reads by value (`const_values`). Then the types table forked again with
//! the live bodies (module docs). Each step is bounded by the one body.

use super::*;

use crate::front2::check::eval_len;
use crate::front2::heads::{HeadError, Want};
use crate::front2::keys::Origin;
use crate::front2::lower_made::{LowerTarget, lower_made};

impl<'n, S: NamesSource<'n>> LazyProgram<'n, S> {
    /// The seam's `check_one` (module docs, step 1). `Ok(None)` is never answered: a key this
    /// program cannot find is [`LazyRefusal::UnknownKey`], since over records an unknown key
    /// may be a member whose container's head is not made yet, not a fn that does not exist.
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
        self.adt_rows(&checked)?;
        self.body_consts(&checked)?;
        self.body_statics(&checked, &function)?;
        self.body_objects(&checked, &function)?;
        self.refork(i, made, &function)?;
        Ok(Some((i, checked)))
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
    fn target_of(&self, key: ItemKey) -> Result<(LowerTarget, Want), LazyRefusal> {
        if let Some(lib) = self.front.keys.library_in(self.front.names, key) {
            return Ok((LowerTarget::Free(lib), self.store.need(lib)));
        }
        match self.front.keys.origin(key) {
            Some(Origin::Member { container, kind: MemberKind::Fn, .. }) => {
                let c = self.front.keys.library_in(self.front.names, *container).ok_or(LazyRefusal::UnknownKey { key: *container })?;
                let want = if self.store.impls.contains_key(container) { Want::MemberFn(c, key) } else { Want::Members(c) };
                Ok((LowerTarget::Member(key), want))
            }
            _ => Err(LazyRefusal::UnknownKey { key }),
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
            let at = match self.front.keys.library(key) {
                Some(lib) => self.placed_of(lib, &path)?.0,
                // A struct's ctor is a member key, placed with its struct (owed, as make_from).
                None => At { file: 0, place: 0 },
            };
            return Ok(self.push(key, FnKind::Ctor, Abi::Rust, at, &path, "", None, None));
        }
        if !self.store.fns.contains_key(&key) {
            let (_, want) = self.target_of(key)?;
            self.make_want(want, key)?;
        }
        let Some(head) = self.store.fns.get(&key) else {
            return Err(refuse(Front2Refusal::NoHead { function: Box::from(path.as_str()) }));
        };
        let (lib, text, header_abi, in_container) = (head.lib, head.text, head.header.abi, head.container.is_some());
        let abi = match header_abi {
            tc::Abi::Rust => Abi::Rust,
            tc::Abi::C => Abi::C,
            tc::Abi::RustCall => Abi::RustCall,
            abi => return Err(refuse(Front2Refusal::Abi { function: Box::from(path.as_str()), abi })),
        };
        let symbols = symbols_of(self.front.names, lib, text, &path).map_err(&refuse)?;
        let (at, foreign) = self.placed_of(lib, &path)?;
        let local = |why: crate::front2::panic_entry::Refused| Front2Refusal::Attrs { function: Box::from(path.as_str()), why: why.form().into() };
        let foreign = foreign.or_else(|| crate::front2::panic_entry::local_abi(&self.store, key));
        let declared_name = crate::front2::panic_entry::local_name(&self.store, key).map_err(local).map_err(&refuse)?;
        let panic_entry = crate::front2::panic_entry::declares(self.front.names, &self.store, key, Alloc::global()).map_err(local).map_err(&refuse)?;
        let alloc_shim = crate::front2::alloc_shim::declares(self.front.names, &self.store, key, Alloc::global())
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
        let abi = match foreign {
            None => abi,
            Some("C" | "C-unwind") => Abi::C,
            Some("Rust") => Abi::Rust,
            Some("rust-call") => Abi::RustCall,
            Some(_) => return Err(refuse(Front2Refusal::Abi { function: Box::from(path.as_str()), abi: header_abi })),
        };
        Ok(self.push(key, kind, abi, at, &path, &symbol, symbols.export.as_deref(), intrinsic))
    }

    #[allow(clippy::too_many_arguments)]
    fn push(&mut self, key: ItemKey, kind: FnKind, abi: Abi, at: At, path: &str, symbol: &str, export: Option<&str>, intrinsic: Option<Intrinsic>) -> usize {
        let i = self.rows.len();
        let path = self.rows.put(path);
        let symbol = self.rows.put(symbol);
        let export = export.map(|e| self.rows.put(e));
        self.rows.key.push(key);
        self.rows.kind.push(kind);
        self.rows.abi.push(abi);
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
    fn placed_of(&self, lib: LibKey, item: &str) -> Result<(At, Option<&'n str>), LazyRefusal> {
        placed(self.front.names, lib).map_err(|_| LazyRefusal::Unanswered { question: "extern_abi", item: Box::from(item) })
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
                let Some(lib) = self.front.keys.library(k) else {
                    return Err(LazyRefusal::Adt { adt: self.refused_shown(k), why: Box::from("no library key") });
                };
                self.make_want(Want::Adt(lib), k).map_err(|e| match e {
                    LazyRefusal::Head { item, why } => LazyRefusal::Adt { adt: item, why },
                    other => other,
                })?;
                if !self.store.adts.contains_key(&k) {
                    return Err(LazyRefusal::Adt { adt: self.refused_shown(k), why: Box::from("made, but no row filed under its key") });
                }
            }
        }
        if let Some(k) = self.store.adts.iter().filter(|(_, a)| a.narrowed).map(|(k, _)| *k).min() {
            let adt = self.refused_shown(k);
            return Err(LazyRefusal::Row { function: adt.clone(), refusal: Box::new(Front2Refusal::Layout { adt }) });
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

    /// The object table of each `dyn` principal `checked`'s types or the heads' table name
    /// (what `make_from` asks over every body: here over this one) that this program has not
    /// made, with every supertrait under it (program.rs `object_tables`, the one home). A
    /// refused trait stops the run, naming it.
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
        let mut tys = self.store.tys.fork_in(Alloc::global()).map_err(|_| LazyRefusal::Table { function: Box::from(function) })?;
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
