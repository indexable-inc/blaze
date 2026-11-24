//! The ONE place rows are added (`&mut self`): `check_one` and what it makes, and `done`.
//!
//! Per body, what `Front2Program::make_from` does for the whole program: the body lowered and
//! checked (`lower_made`, `check_body`), a row for each fn it calls (as `program/tabled.rs`
//! `row` makes one, without a body), the row of each ADT its types name (make_from's rounds),
//! each constant it reads by value (`const_values`). The build reads the same body and
//! type overlay that the checker used.

use super::*;

use crate::front2::check::eval_len;
use crate::front2::program::head_abi::{BuildAbi, head_abi};
use crate::front2::heads::{HeadError, Want};
use crate::front2::keys::Origin;
use crate::front2::lower_made::{LowerTarget, lower_made};
use crate::run::make_one::LoweringCounts;

#[derive(Default)]
pub(super) struct Lowerings {
    count: LoweringCounts,
    #[cfg(test)]
    refork: Option<PortableTys<'static>>,
}

/// Where a body's checked columns come from in [`LazyProgram::check_one_key_by`]: chosen by the
/// typing seam's type (run/make_one.rs `Typing`: `SecondCheck` checks here, `FromRecord` reads
/// the check's record), each instantiation one arm.
pub(super) enum Typer<'r> {
    /// The body checked here (`check_body`): today's second check.
    Check,
    /// The body's columns read from the check's record, never checked here; a gap is a typed
    /// refusal naming the body ([`LazyRefusal::Typed`]).
    Record(&'r mut dyn super::typed::TypedRecords),
}

impl<'n, S: PositionSource<'n>> LazyProgram<'n, S> {
    /// The seam's `check_one` (module docs, step 1). A declaration with no body answers
    /// `Ok(None)`. A key the records search cannot find gives [`LazyRefusal::NotFound`].
    pub(super) fn check_one_key(&mut self, key: ItemKey) -> Result<Option<(usize, Checked<'static>)>, LazyRefusal> {
        self.check_one_key_by(key, Typer::Check)
    }

    /// [`Self::check_one_key`] with the body's checked columns from `typer`: lowered here as
    /// always (the build reads the run's lowering), then checked here or read from the check's
    /// record, then tabled the same way. The record arm never calls `check_body`.
    pub(super) fn check_one_key_by(&mut self, key: ItemKey, typer: Typer<'_>) -> Result<Option<(usize, Checked<'static>)>, LazyRefusal> {
        if let Some(&i) = self.rows.by_key.get(&key)
            && self.rows.checked[i as usize]
        {
            return Err(LazyRefusal::AskedTwice { key });
        }
        let (target, want) = self.target_of(key)?;
        let function: Box<str> = self.shown(key).into_boxed_str();
        let prepared = self.prepared;
        let lowered = {
            let mut r = self.records(true);
            lower_made(&mut r, prepared, target)
        };
        let mut made = match lowered {
            Ok(body) => {
                self.lowerings.count.bodies_lowered += 1;
                body
            }
            Err(BodyError::NoBody(owner)) if owner == key => {
                // A free fn can reach lowering before its head has been made. Its declaration
                // decides whether the missing body is expected.
                if !self.store.fns.contains_key(&key) {
                    self.make_want(want, key, "check_one_key_by (a body-less fn's head)")?;
                }
                if self.store.fns.get(&key).is_some_and(|head| !head.has_body) {
                    return Ok(None);
                }
                return Err(LazyRefusal::Lower { function, error: BodyError::NoBody(owner) });
            }
            Err(error) => return Err(LazyRefusal::Lower { function, error }),
        };
        let checked = match typer {
            Typer::Check => {
                let checked = if made.typeck.inline_fors.is_empty() {
                    let view = made.lowered.view();
                    let spell = match Spelling::of(&view) {
                        Ok(spell) => spell,
                        Err(_) => return Err(LazyRefusal::Spell { function }),
                    };
                    let scope = made.scope(&spell);
                    let mut r = self.records(true);
                    check_body(&mut r, &made, &scope)
                } else {
                    let mut r = self.records(true);
                    crate::front2::inline_for::unroll_concrete_body(&mut r, prepared, &mut made)
                };
                let checked = checked.map_err(|error| self.check_refusal(&function, error))?;
                self.counts.checked += 1;
                checked
            }
            // Read, not checked: `counts.checked` stays as it is (a run from records checks no
            // body, and the count says so).
            Typer::Record(records) => {
                let checked = match records.typed(key) {
                    Ok(checked) => checked,
                    Err(gap) => return Err(LazyRefusal::Typed { function, gap }),
                };
                if !made.typeck.inline_fors.is_empty() {
                    let expanded = {
                        let mut r = self.records(true);
                        crate::front2::inline_for::expand_concrete_body(&mut r, prepared, &mut made)
                    };
                    expanded.map_err(|error| self.check_refusal(&function, error))?;
                }
                self.file_read_check(key, want, &made, &checked, &function)?;
                checked
            }
        };
        let i = self.row(key, false)?;
        self.rows.checked[i] = true;
        self.table_body_facts(i, key, made, &checked, &function)?;
        Ok(Some((i, checked)))
    }

    /// Tables everything the build of row `i`'s checked body asks the `&self` seam getters
    /// (ADTs, callees, async/fmt items, constants, statics, trait objects, coroutine recipes,
    /// and the live body itself). Shared between `check_one_key_by` and on-demand generic
    /// `inline for` specialization (`specialize_inline_for_row`).
    fn table_body_facts(&mut self, i: usize, fmt_owner: ItemKey, made: Body<'n>, checked: &Checked<'static>, function: &str) -> Result<(), LazyRefusal> {
        // THE ONE RULE for what a body's columns name (both arms): every item they name is
        // filed and its head made HERE, from the columns, never left to the check's side
        // effects. A check files what it asks as it goes (an impl's head files its members,
        // keys.rs `Keys::member`; an ADT's head its ctors); a body read from its record was
        // not checked here, so nothing did (d106 `a_run_from_typed_records_..`, "async
        // rung": `main`'s callee, a member of an impl no other body here had asked, had no row
        // path). Over a checked body each step finds its item already filed and does nothing.
        // The ADT heads first: a callee that is a ctor is filed by its ADT's head.
        self.adt_rows(checked)?;
        // Each fn the body names as a value: filed (lazy/locate.rs), so `bind_internal` and
        // the walk's `check_one` of it read a key the map knows.
        for &def in checked.expr_defs.column::<1>() {
            if let tc::Def::Fn(k) = def {
                self.locate(k)?;
            }
        }
        // Each fn the body calls: its head row (the build reads a call's ABI and signature
        // from the item the call names). A trait's fn called by its trait's path is tabled as
        // declared, as make_from tables it.
        for &callee in checked.callees.column::<1>() {
            self.callee_row(callee)?;
        }
        self.async_rows(&made)?;
        self.fmt_rows(&made, fmt_owner, function)?;
        // Again after the callees' and async items' heads: their types name ADTs too (the
        // rounds over the store's table pick them up; nothing missing is nothing made).
        self.adt_rows(checked)?;
        self.body_consts(checked)?;
        self.body_statics(checked, function)?;
        self.body_objects(checked, function)?;
        // Its async blocks' state recipes (coroutine_recipes.rs, as `Front2Program::make_from`
        // makes them), AFTER its callees' heads are in the store: the lowering relates a call
        // through its callee's signature (an `async fn` called with an `.await`, its future's
        // lifetimes), and with no head there the call is refused `ElisionAmbiguous`. A body
        // with no async block asks nothing.
        self.body_recipes(&made, checked, function)?;
        self.keep_body(i, made, function)?;
        Ok(())
    }

    /// On-demand specialization of a generic `inline for` function at row `orig_i` for concrete
    /// ground `args` (in `ground`): unrolls and checks the monomorphized body, tables all its
    /// dependencies and head facts into a new row, caches it in `self.inline_for_specs`, and
    /// hands over `(spec_row, orig_key, checked)`. Returns `Ok(None)` if row `orig_i` has no
    /// unexpanded generic `inline for` loops or `args` cannot be specialized.
    pub(super) fn specialize_inline_for_row(
        &mut self,
        orig_i: usize,
        args: &[PTy],
        ground: PTys<'_>,
    ) -> Result<Option<(usize, ItemKey, Checked<'static>)>, LazyRefusal> {
        if args.is_empty() {
            return Ok(None);
        }
        let has_inline_for = self.live_of(orig_i).is_some_and(|l| !l.made.typeck.inline_fors.is_empty());
        if !has_inline_for {
            return Ok(None);
        }
        let Some(&orig_key) = self.rows.key.get(orig_i) else {
            return Ok(None);
        };
        let function = self.shown(orig_key).into_boxed_str();
        let prepared = self.prepared;
        let specialized = {
            let mut r = self.records(true);
            crate::front2::inline_for::specialize_one_generic_inline_for(&mut r, prepared, orig_key, ground, args)
        };
        let Some(spec) = specialized.map_err(|error| self.check_refusal(&function, error))? else {
            return Ok(None);
        };
        let spec_i = self.push_specialized_row(orig_i, spec.spec_key, &function)?;
        if let Some(flag) = self.rows.checked.get_mut(spec_i) {
            *flag = true;
        }
        self.counts.checked += 1;
        self.table_body_facts(spec_i, orig_key, spec.body, &spec.checked, &function)?;
        self.inline_for_specs.push(crate::front2::inline_for::SpecRow { orig: orig_key, args: spec.args, row: spec_i });
        Ok(Some((spec_i, orig_key, spec.checked)))
    }

    fn push_specialized_row(&mut self, orig_i: usize, spec_key: ItemKey, function: &str) -> Result<usize, LazyRefusal> {
        let i = self.rows.len();
        let idx = u32::try_from(i).map_err(|_| LazyRefusal::Table { function: function.into() })?;
        let missing = || LazyRefusal::Table { function: function.into() };
        let abi = *self.rows.abi.get(orig_i).ok_or_else(missing)?;
        let nounwind = *self.rows.nounwind.get(orig_i).ok_or_else(missing)?;
        let track_caller = *self.rows.track_caller.get(orig_i).ok_or_else(missing)?;
        let at = *self.rows.at.get(orig_i).ok_or_else(missing)?;
        let path = *self.rows.path.get(orig_i).ok_or_else(missing)?;
        let symbol = *self.rows.symbol.get(orig_i).ok_or_else(missing)?;
        let intrinsic = *self.rows.intrinsic.get(orig_i).ok_or_else(missing)?;
        self.rows.key.push(spec_key);
        self.rows.kind.push(FnKind::Free);
        self.rows.abi.push(abi);
        self.rows.nounwind.push(nounwind);
        self.rows.track_caller.push(track_caller);
        self.rows.at.push(at);
        self.rows.path.push(path);
        self.rows.symbol.push(symbol);
        self.rows.export.push(None);
        self.rows.intrinsic.push(intrinsic);
        self.rows.live.push(None);
        self.rows.checked.push(false);
        self.rows.by_key.insert(spec_key, idx);
        self.counts.rows += 1;
        Ok(i)
    }

    /// Decides where the process starts, once, by THE start rule (program.rs `start_of`, as
    /// run/special.rs `roots` takes it): an exported entry; else a Rust root's `fn main` (found
    /// by run_entry.rs `root_main`) through the linked `#[lang = "start"]`, its head read into
    /// THIS program's store (start.rs `rust_start`: `T`, `main`'s return type, is a type of the
    /// store's table, the one the back half reads); else `main` called directly. What the seam's
    /// `rust_start` answers. Reads crate 0's fn rows and their attributes, the linked crates'
    /// lang rows, and for a Rust start `main`'s head; no body.
    ///
    /// `start` is what the front's roots say (run/special.rs `MakerStart`): `ByRule` is the
    /// above, a program's run; `Direct(f)` is a test's run, `f` called directly, and the rule
    /// (and so the crate's `fn main`) is not asked at all.
    pub fn decide_start(&mut self, start: crate::run::special::MakerStart) -> Result<(), crate::run::special::RootsError> {
        use crate::front2::start::{Entry2, rust_start};
        use crate::run::special::{MakerStart, RootsError};
        let names = self.front.names;
        if let MakerStart::Direct(entry) = start {
            self.start = Some(Entry2::Direct(entry));
            return Ok(());
        }
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

    /// The seam's `head_row`: fn item `key`'s row from its head, made once, no body lowered or
    /// checked. Its head is made first by THE one rule for a key's want (front2 fill.rs
    /// `want_of_key`): a fn's signature, an impl member's `MemberFn`, a trait's members, and
    /// for a ctor its ADT's head (a tuple struct's ctor is a member key with no library key,
    /// which `target_of` refuses, so it is not asked here). Then the row as a callee's
    /// (`callee_row`: a trait's fn called by its trait's path tabled as declared, a ctor by
    /// `row`'s ctor arm, an `extern` fn and an intrinsic as `row` places them).
    ///
    /// The fn item an instance's ARGUMENTS hold is the case no body's check covers (t1530, 36
    /// tests: `Result::map` at `NonZero::<u64>::new`, `Option::map` at `Into::into`, fastrand's
    /// `try_with_rng(Rng::fork)`, a fn handed to `OnceLock::get_or_init`): the body that named
    /// it as a value was found by its record, so nothing of this run made its head and the
    /// build's `Adts::abi` had no answer.
    pub(super) fn head_row_key(&mut self, key: ItemKey) -> Result<usize, LazyRefusal> {
        if let Some(&i) = self.rows.by_key.get(&key) {
            return Ok(i as usize);
        }
        self.locate(key)?;
        if !self.store.fns.contains_key(&key) && !self.store.ctors.contains_key(&key) {
            let want = crate::front2::fill::want_of_key(self.front.names, &self.front.keys, key)
                .map_err(|e| LazyRefusal::Head { item: self.refused_shown(key), why: format!("head_row (lazy/fill.rs) of a fn item: {e}").into_boxed_str() })?;
            self.make_want(want, key, "head_row (a fn item with no body checked)")?;
        }
        self.callee_row(key)
    }

    /// The seam's `done`: row `i`'s body is dropped (its head facts stay).
    pub(super) fn done_row(&mut self, i: usize) {
        let Some(slot) = self.rows.live.get_mut(i).and_then(Option::take) else { return };
        self.live[slot as usize] = None;
        self.counts.live -= 1;
    }

    /// How fn `key`'s body is lowered and its head made.
    /// A key the key map does not know yet is first found and filed from the records
    /// (lazy/locate.rs: an item row, a member of a container whose head is not made, an item
    /// declared in a body); the container's head is then the want, made on demand.
    fn target_of(&mut self, key: ItemKey) -> Result<(LowerTarget, Want), LazyRefusal> {
        self.locate(key)?;
        if let Some(lib) = self.front.keys.library_in(self.front.names, key) {
            // A free body owner's head is its own key's want (a fn's, constant's or static's
            // signature, an item declared in a body by the kind its lowering filed: fill.rs
            // `want_of_key`, the one rule), never `Store::need`'s, which asks a type's or
            // trait's head and refuses a fn's key by name.
            let want = crate::front2::fill::want_of_key(self.front.names, &self.front.keys, key)
                .map_err(|e| LazyRefusal::Head { item: self.refused_shown(key), why: format!("{e}").into_boxed_str() })?;
            return Ok((LowerTarget::Free(lib), want));
        }
        match self.front.keys.origin(key) {
            // A fn or an associated constant with a body (a body owner, reach/initializers.rs:
            // its closures are built at its row): its head by THE CHECK'S OWN RULE, heads/
            // member_fn.rs `member_head_want` (what body.rs asks for a member it names): an
            // impl's fn its own `MemberFn` (the impl's head first), an impl's constant the
            // impl's head, a trait's fn or constant its trait's members. Never `Store::need`
            // of the container, which asks an impl for a type's or trait's head (`Want::Head`)
            // and is refused "is an impl" (heads.rs `step`'s last arm).
            Some(Origin::Member { container, kind: kind @ (MemberKind::Fn | MemberKind::Const), .. }) => {
                let (container, kind) = (*container, *kind);
                let Some(want) = crate::front2::heads::member_head_want(self.front.names, &self.front.keys, key) else {
                    return Err(LazyRefusal::NotFound {
                        key,
                        krate: self.refused_shown(key),
                        searched: format!("a {kind:?} member of {:032x}, which is filed with no library key or is no trait or impl (heads/member_fn.rs `member_head_want`)", container.0).into_boxed_str(),
                    });
                };
                Ok((LowerTarget::Member(key), want))
            }
            Some(Origin::Member { kind, .. }) => Err(LazyRefusal::NotFound { key, krate: self.refused_shown(key), searched: format!("it is a member of kind {kind:?}, not a fn with a body").into_boxed_str() }),
            Some(Origin::Item(_)) | None => Err(LazyRefusal::NotFound { key, krate: self.refused_shown(key), searched: Box::from("the key map filed it with no library key") }),
        }
    }

    /// A body's checked columns READ from its record, filed in the store as a clean check of
    /// it files them (front2 body_reads.rs `check_body_keyed` and `check_body_filed_as`, after
    /// `check_rounds`), so a later question about the body's coroutines or closures has the
    /// same answer whichever arm made the columns (the module doc of lazy/typed.rs, "Body-local
    /// items"):
    ///
    /// - its async blocks' and coroutines' rows and its opaques' hidden types: front2
    ///   opaque_auto.rs `file` into `Store::hidden`, the world's answer to
    ///   `Question::Coroutine(owner, n)` (world_items.rs `coroutine_once`) and to an opaque's
    ///   auto traits. It makes the exact interiors from the body and its witnesses
    ///   (exact_interior.rs), as on the check side, and reads the owner's head, made first.
    /// - its closures' kinds, signatures and captures: foreign_closures.rs `ClosureRows::file`
    ///   into `Store::closures`, the answer to `Question::Closure(owner, n)`.
    ///
    /// Not filed: the read list (`Store::reads`): a record's body is not checked here, it
    /// has no read list of this run, and nothing a run makes reads one.
    fn file_read_check(&mut self, key: ItemKey, want: Want, made: &crate::front2::body::Body<'_>, checked: &Checked<'_>, function: &str) -> Result<(), LazyRefusal> {
        if !self.store.fns.contains_key(&key) {
            self.make_want(want, key, "file_read_check (the coroutine owner's head)")?;
        }
        let filed = {
            let mut r = self.records(true);
            crate::front2::opaque_auto::file(&mut r, made, checked)
        };
        filed.map_err(|error| self.check_refusal(function, error))?;
        let store = &mut self.store;
        store.closures.file(&made.typeck.view(tc::PTys::EMPTY), checked, &mut store.tys);
        Ok(())
    }

    /// `want` made, each constant its head names evaluated as met (at most `MOST_ADT_LENS`),
    /// as make_from's ADT rounds do. `asker` names the step of this file that asked (rule
    /// 19): a refusal says who asked which want for which item.
    fn make_want(&mut self, want: Want, item: ItemKey, asker: &'static str) -> Result<(), LazyRefusal> {
        let mut lens = 0;
        loop {
            let names = self.front.names;
            match self.store.ensure(names, &mut self.front.keys, want) {
                Err(HeadError::Item(_, Why::NeedConst(c, _))) if lens < MOST_ADT_LENS => {
                    lens += 1;
                    let mut r = self.records(true);
                    if let Err(e) = eval_len(&mut r, c) {
                        return Err(LazyRefusal::Head { item: self.refused_shown(item), why: format!("{asker} asked {want:?}: the constant {c:x} its head names: {e:?}").into_boxed_str() });
                    }
                }
                Err(e) => {
                    let shown = crate::front2::item_shown::head_error_shown(names, &e);
                    return Err(LazyRefusal::Head { item: self.refused_shown(item), why: format!("{asker} (lazy/fill.rs) asked {want:?}: {shown}").into_boxed_str() });
                }
                Ok(()) => return Ok(()),
            }
        }
    }

    /// The row of `callee`, a fn a checked body calls: filed and its head made first (the
    /// container's head for a member: an impl's lands in `store.impls`), so whether it is a
    /// trait's fn called as declared is read from what is made, not from what a check of the
    /// caller happened to make before. A ctor's head is its ADT's (`adt_rows`, run before).
    fn callee_row(&mut self, callee: ItemKey) -> Result<usize, LazyRefusal> {
        if let Some(&i) = self.rows.by_key.get(&callee) {
            return Ok(i as usize);
        }
        self.locate(callee)?;
        if !self.store.fns.contains_key(&callee) && !self.store.ctors.contains_key(&callee) {
            let (_, want) = self.target_of(callee)?;
            self.make_want(want, callee, "callee_row")?;
        }
        let of_trait = matches!(self.front.keys.origin(callee), Some(Origin::Member { container, kind: MemberKind::Fn, .. }) if !self.store.impls.contains_key(container));
        self.row(callee, of_trait)
    }

    /// Fn `key`'s row, made once: its head facts as `program/tabled.rs` `row` reads them, with
    /// no body (a body is `check_one`'s). Returns its index. A key the map does not know yet
    /// is filed first (lazy/locate.rs), as `target_of` files it: its row path is read from
    /// the key map.
    fn row(&mut self, key: ItemKey, of_trait: bool) -> Result<usize, LazyRefusal> {
        if let Some(&i) = self.rows.by_key.get(&key) {
            return Ok(i as usize);
        }
        self.locate(key)?;
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
            return Ok(self.push(key, FnKind::Ctor, ctor, false, blaze_lir::func::FnAttrs::default(), at, &path, "", None, None));
        }
        if !self.store.fns.contains_key(&key) {
            let (_, want) = self.target_of(key)?;
            self.make_want(want, key, "row")?;
        }
        let Some(head) = self.store.fns.get(&key) else {
            return Err(refuse(Front2Refusal::NoHead { function: Box::from(path.as_str()) }));
        };
        let (lib, text, in_container, has_body) = (head.lib, head.text, head.container.is_some(), head.has_body);
        let values_at = crate::front2::links::values_key(self.front.names, &self.store, lib);
        let symbols = symbols_of(self.front.names, lib, values_at, text, &path).map_err(&refuse)?;
        let (at, foreign) = self.placed_of(key, lib, &path)?;
        let local = |why: crate::front2::panic_entry::Refused| Front2Refusal::Attrs { function: Box::from(path.as_str()), why: why.form().into() };
        let foreign = foreign.or_else(|| crate::front2::panic_entry::local_abi(&self.store, key));
        if let Some(llvm) = seam_rules::llvm_row(foreign, symbols.link_name.as_deref(), &path).map_err(&refuse)? {
            return Ok(self.push(key, llvm.kind, llvm.build, llvm.track_caller, symbols.fn_attrs, at, &path, llvm.symbol, symbols.export.as_deref(), Some(llvm.intrinsic)));
        }
        let declared_name = crate::front2::panic_entry::local_name(&self.store, key).map_err(local).map_err(&refuse)?;
        let alloc = self.store.tys.alloc();
        let panic_entry = crate::front2::panic_entry::declares(self.front.names, &self.store, key, alloc).map_err(local).map_err(&refuse)?;
        let alloc_shim = crate::front2::alloc_shim::declares(self.front.names, &self.store, key, alloc)
            .map_err(|why| refuse(Front2Refusal::Attrs { function: Box::from(path.as_str()), why: why.detail() }))?;
        let bound = seam_rules::bound_decl_row(self.front.names, &self.store, key, panic_entry, alloc_shim, &path).map_err(&refuse)?;
        // A call of an allocator declaration is made with the ABI of the fn it runs (front_end.rs
        // `fn_abi`), which the builder asks at the call, before that fn is called: its head is
        // made here, with the declaration's row (no check of the caller made it: the caller
        // names the declaration alone).
        if alloc_shim.is_some()
            && let Some(default) = crate::front2::alloc_shim::bound(self.front.names, &self.front.index, &self.store, key, alloc)
            && !self.store.fns.contains_key(&default)
        {
            let (_, want) = self.target_of(default)?;
            self.make_want(want, default, "row (an allocator declaration's bound fn)")?;
        }
        let (kind, symbol) = if let Some(bound) = bound {
            bound
        } else if foreign.is_some() && !in_container {
            let symbol = symbols.link_name.as_deref().or(declared_name).unwrap_or(row_path.ident());
            (FnKind::Foreign, symbol)
        } else {
            let kind = seam_rules::rust_row_kind(of_trait, has_body || symbols.intrinsic, &path).map_err(&refuse)?;
            (kind, "")
        };
        // A `#[rustc_intrinsic]` fn is built by the ONE rule (front2/intrinsic_rule.rs): the
        // table's row when the table names it; else, when its declaration has a fallback body
        // (and is not `#[rustc_intrinsic_must_be_overridden]`), from that body like any fn: no
        // table entry, the row `Free`, and `check_one` makes the body on its first call (core's
        // `integer_min`, `if a < b { a } else { b }`); else refused, naming which and where.
        let intrinsic = if symbols.intrinsic {
            use crate::front2::intrinsic_rule::{Built, built, declared_at};
            match built(row_path.ident(), symbols.intrinsic_declared) {
                Ok(Built::Table(which)) => Some(Intrinsic::Table(which)),
                Ok(Built::FallbackBody) => None,
                Err(unbuilt) => {
                    let at = declared_at(self.front.names, lib);
                    return Err(refuse(Front2Refusal::Intrinsic { function: Box::from(path.as_str()), unbuilt, at }));
                }
            }
        } else if alloc_shim == Some(crate::front2::alloc_shim::Shim::Marker) {
            match blaze_build::intrinsics::Which::named(crate::front2::alloc_shim::Shim::Marker.declared()) {
                Ok(which) => Some(Intrinsic::Table(which)),
                Err(_) => {
                    let at = crate::front2::intrinsic_rule::declared_at(self.front.names, lib);
                    return Err(refuse(Front2Refusal::Intrinsic { function: Box::from(path.as_str()), unbuilt: crate::front2::intrinsic_rule::Unbuilt::NoBody, at }));
                }
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
        Ok(self.push(key, kind, build, symbols.track_caller, symbols.fn_attrs, at, &path, symbol, symbols.export.as_deref(), intrinsic))
    }

    #[allow(clippy::too_many_arguments)]
    fn push(&mut self, key: ItemKey, kind: FnKind, build: BuildAbi, track_caller: bool, fn_attrs: blaze_lir::func::FnAttrs, at: At, path: &str, symbol: &str, export: Option<&str>, intrinsic: Option<Intrinsic>) -> usize {
        let i = self.rows.len();
        let path = self.rows.put(path);
        let symbol = self.rows.put(symbol);
        let export = export.map(|e| self.rows.put(e));
        self.rows.key.push(key);
        self.rows.kind.push(kind);
        self.rows.abi.push(build.abi);
        self.rows.nounwind.push(build.nounwind);
        self.rows.track_caller.push(track_caller);
        self.rows.fn_attrs.push(fn_attrs);
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
    /// A check of `function`'s body that did not finish clean, as refused: a gap naming an item
    /// declared in a body as [`LazyRefusal::LocalNotBuilt`] (where it is written, as a user
    /// reads it), anything else as [`LazyRefusal::Check`].
    fn check_refusal(&self, function: &str, error: CheckFail) -> LazyRefusal {
        use crate::front2::world_items::Gap;
        let local = match &error {
            CheckFail::Gap { gaps, .. } => gaps.iter().find_map(|g| match g {
                Gap::Local(l) => Some((*l, gaps.len() - 1)),
                _ => None,
            }),
            _ => None,
        };
        // What the gap names: a local item (its head: the body it is declared in, and its head
        // as written), or a row of a body named by the body's key (a `use`: the body whose
        // rows hold it, and its text as written, read below from its file).
        let Some((gap, others)) = local else {
            // A diagnostic: its words and its `file:line` (rule 19: the checker's range is
            // relative to the body's own item, rule 71; a body of an item declared in a body is
            // placed through the items around it, as a gap is below).
            let placed = match &error {
                CheckFail::Diags { owner, diags, .. } => diags
                    .first()
                    .and_then(|(_, range, _)| self.file_line(*owner, range.start, range.end.saturating_sub(range.start))),
                _ => None,
            };
            if let Some((at, _)) = placed {
                let function = Box::from(function);
                return match error {
                    CheckFail::Diags { diags, .. } => {
                        let others = diags.len().saturating_sub(1);
                        match diags.into_iter().next() {
                            Some((code, _, about)) => LazyRefusal::Diagnostic { function, code: code.code(), about, at: at.into_boxed_str(), others },
                            None => LazyRefusal::Diagnostic { function, code: "", about: Box::from(""), at: at.into_boxed_str(), others: 0 },
                        }
                    }
                    error => LazyRefusal::Check { function, error },
                };
            }
            return LazyRefusal::Check { function: Box::from(function), error };
        };
        let head = self.store.locals.head(crate::front2::keys::checker_of(gap.item)).copied();
        let (enclosing_key, head_words, row) = match (head, self.store.locals.row_of_gap(&gap)) {
            (Some(h), _) => {
                // The head as written: its first line that is no attribute or comment, to its `{`.
                let first = h.text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("//")).unwrap_or("");
                (h.enclosing, Some(first.split('{').next().unwrap_or("").trim()), None)
            }
            (None, Some((body, row))) => (body, None, Some(row)),
            (None, None) => return LazyRefusal::Check { function: Box::from(function), error },
        };
        let found = self.file_line(enclosing_key, gap.range.start, gap.range.end - gap.range.start);
        let (at, written) = match found {
            Some((at, written)) => (at, written),
            None => (format!("at {}..{} of its enclosing item, which is in no file the source answers", gap.range.start, gap.range.end), None),
        };
        // The words a user reads: the head's first line; a row's text in its file, its runs of
        // white space one space each (its attributes with it: a `#[cfg(..)]` shows which target
        // it is for); a row in no file: its path (`use a::b` names what it binds).
        let mut words = String::new();
        match (head_words, written, row) {
            (Some(w), _, _) => words.push_str(w),
            (None, Some(t), _) => {
                for (i, w) in t.split_whitespace().enumerate() {
                    if i > 0 {
                        words.push(' ');
                    }
                    words.push_str(w);
                }
            }
            (None, None, Some(r)) => {
                words.push_str(if r.abs { "use ::" } else { "use " });
                for (i, s) in r.path.iter().enumerate() {
                    if i > 0 {
                        words.push_str("::");
                    }
                    words.push_str(s);
                }
            }
            (None, None, None) => {}
        }
        let words = words.into_boxed_str();
        // A module's item by its whole path (item_shown.rs); one declared in a body has none,
        // so by the names it is written under (`g::h`).
        let enclosing = match self.store.locals.head(enclosing_key) {
            Some(_) => format!("`{}`", self.shown(enclosing_key)).into_boxed_str(),
            None => self.refused_shown(enclosing_key),
        };
        LazyRefusal::LocalNotBuilt { function: Box::from(function), enclosing, form: gap.form, head: words, at: at.into_boxed_str(), others }
    }

    /// `file:line` of byte `start` of the item `outer`'s text (a range relative to its item,
    /// rule 71), and the file's `len` bytes there: an item declared in a body is at its own
    /// range in ITS enclosing one, out to a module's item, whose file the source names. Each
    /// step out leaves one local item: at most as many steps as there are. `None`: an item in
    /// no file the source answers (an expansion's text keeps only its outermost call's file).
    fn file_line(&self, outer: ItemKey, start: u32, len: u32) -> Option<(String, Option<&'n str>)> {
        let names = self.front.names;
        let keys = &self.front.keys;
        let (mut outer, mut start) = (outer, start);
        for _ in 0..=self.store.locals.kept() {
            let Some(up) = self.store.locals.head(outer) else { break };
            start = start.checked_add(up.range.start)?;
            outer = up.enclosing;
        }
        let lib = keys.library_in(names, outer)?;
        let place = names.place_of(lib)?;
        let blaze_scope::resolve::At::File(i) = place.at else { return None };
        let shown = names.file_shown(place.krate, i)?;
        let (line, _) = super::seam_rules::source_position(names, keys, &self.store, outer, start).ok()?;
        // The text written at the range, when the range is the file's own (an expansion's row
        // is at its outermost call: the call's text).
        let written = names.item_start(lib).and_then(|(_, item_start)| {
            let from = item_start.checked_add(start)? as usize;
            let to = from.checked_add(len as usize)?;
            names.file_text(place.krate, i)?.get(from..to)
        });
        Some((format!("{shown}:{line}"), written))
    }

    pub(super) fn shown(&self, k: ItemKey) -> String {
        crate::front2::row_path::shown_name(self.front.names, &self.front.keys, &self.store, k)
    }

    /// `k` for a refusal (rule 19): its whole path and its key (front2 item_shown.rs
    /// `ItemShown`); [`Self::shown`] stays the bare name a row's path is made from.
    pub(super) fn refused_shown(&self, k: ItemKey) -> Box<str> {
        self.item_shown(k).boxed()
    }

    /// `k` for a person (front2 item_shown.rs `ItemShown`, the one printer): its path (a
    /// body-local item's through the item whose body declares it, `ItemShown::of_in`), or
    /// for a path-less item where it is written, and its key.
    pub fn item_shown(&self, k: ItemKey) -> crate::front2::item_shown::ItemShown {
        crate::front2::item_shown::ItemShown::of_in(self.front.names, &self.front.keys, &self.store, k)
    }

    /// Byte `site` of item `item`'s text (item-relative, as a code record's trap row holds it,
    /// rule 71) for a person: its file as the program observes it (`file!()`), and its line and
    /// column today (seam_rules.rs, THE POSITION RULE). `None`: the item's text is in no file
    /// (a macro's expansion) or the names do not place it.
    pub fn site_shown(&self, item: ItemKey, site: u32) -> Option<(&'n str, u32, u32)> {
        let file = super::seam_rules::item_file(self.front.names, &self.front.keys, &self.store, item)?;
        let (line, column) = super::seam_rules::source_position(self.front.names, &self.front.keys, &self.store, item, site).ok()?;
        Some((file, line, column))
    }

    /// The row of every ADT `checked`'s types and the heads' table name, in rounds as a row's
    /// fields name more (make_from's loop, over ONE body).
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
        Ok(())
    }

    /// The ADT `k` (library item `lib`) made and filed under its key, or refused by name.
    fn adt_row(&mut self, lib: LibKey, k: ItemKey) -> Result<(), LazyRefusal> {
        self.make_want(Want::Adt(lib), k, "adt_row").map_err(|e| match e {
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
            // The lang item's whole record by the one rule (by_origin.rs `item_want` of
            // `kind_of_lib`): a library-keyed member's is its OWNER's, a trait's members or an
            // impl's head (the ladder's `into_future` is the blanket impl's fn,
            // `<impl<F: Future> IntoFuture for F>::into_future`); a free fn's its own. Never
            // `Store::need` of the owner: that asks an impl for a type's head and was refused
            // "is an impl" (the async rung, d106 `a_run_from_typed_records_..`).
            let names = self.front.names;
            let want = crate::front2::by_origin::kind_of_lib(names, lib)
                .and_then(|kind| crate::front2::by_origin::item_want(names, kind))
                .map_err(|e| LazyRefusal::Head { item: self.refused_shown(k), why: format!("async_rows: the lang item `{name}`: {e}").into_boxed_str() })?;
            self.make_want(want, k, "async_rows (a lang fn's container)")?;
            // An impl's fn: its own signature, now that its impl's head filed it (heads/
            // member_fn.rs `member_fn_want`, the check's rule). No head filed under `k` after
            // this is refused where it is needed, by name (the build's `NoAbi`).
            if !self.store.fns.contains_key(&k)
                && let Some(own) = crate::front2::heads::member_fn_want(names, &self.front.keys, k)
            {
                self.make_want(own, k, "async_rows (a lang fn of an impl)")?;
            }
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

    /// For each formatting trait the body's `format_args!` placeholders ask, core's
    /// `rt::Argument` constructor the builder calls (which no checked body names: the checker
    /// records no callee for a placeholder), asked by fmt_template.rs `argument_new` (the one
    /// lookup both programs make, its answer kept in the impl index where the seam's
    /// `fmt_argument_new` reads it) and tabled as a callee. Not found is refused naming the
    /// constructor and the body (rule 19), as the eager walk files it failed.
    fn fmt_rows(&mut self, made: &crate::front2::body::Body<'_>, key: ItemKey, function: &str) -> Result<(), LazyRefusal> {
        for t in crate::front2::fmt_template::placeholder_traits(made) {
            let found = {
                let mut r = self.records(true);
                crate::front2::fmt_template::argument_new(&mut r, key, t)
            };
            let k = found.map_err(|why| LazyRefusal::FmtArgument { function: Box::from(function), why })?;
            self.row(k, false)?;
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
            let static_ = self.static_shown(k);
            return Err(LazyRefusal::Row { function: Box::from(function), refusal: Box::new(Front2Refusal::Static { static_, why }) });
        }
        let crate::front2::statics::Statics { values, kinds, foreign } = made;
        // A static an `extern` block declares: its C symbol, no record (statics.rs
        // `static_values`).
        for (k, symbol) in foreign {
            if let Err(at) = self.statics.foreign.binary_search_by_key(&k, |(x, _)| *x) {
                self.statics.foreign.insert(at, (k, symbol));
            }
        }
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

    /// Static `item` as a refusal names it (front2 static_shown.rs, the one home, as the eager
    /// program.rs `made_statics` names it): a module-level static's path; one declared in a
    /// body (a static a macro writes in a block: tracing's `callsite2!`) by its written head and
    /// the item whose body declares it, never "an item the names source holds no path for".
    pub fn static_shown(&self, item: ItemKey) -> Box<str> {
        crate::front2::static_shown::static_shown_in(self.front.names, &self.front.keys, &self.store, item).into_boxed_str()
    }


    /// Make static `item`'s value and kind available for its run record (code_records.rs
    /// `statics`): the one this program made when a body named it, or evaluated now, alone
    /// (statics.rs `one_static`; the statics its value points at are the walk's own rows, each
    /// asked by its record, never here). A refusal names the static.
    pub fn make_static_value(&mut self, item: ItemKey) -> Result<blaze_build::StaticItem, LazyRefusal> {
        if let Some(kind) = self.statics.kind(item) {
            Ok(kind)
        } else {
            let made = {
                let mut r = self.records(true);
                crate::front2::statics::one_static(&mut r, item)
            };
            let (record, kind) = made.map_err(|why| {
                let static_ = self.static_shown(item);
                LazyRefusal::Row { function: static_.clone(), refusal: Box::new(Front2Refusal::Static { static_, why }) }
            })?;
            if let Err(at) = self.statics.values.binary_search_by_key(&item, |(x, _)| *x) {
                self.statics.values.insert(at, (item, record));
            }
            if let Err(at) = self.statics.kinds.binary_search_by_key(&item, |(x, _)| *x) {
                self.statics.kinds.insert(at, (item, kind));
            }
            Ok(kind)
        }
    }

    /// Static `item`'s value after [`Self::make_static_value`] has made it available.
    /// This read-only half keeps the value borrow separate from the mutable evaluation above, so
    /// a caller can resolve the function instances named by the value while it builds the entry.
    pub fn static_value(&self, item: ItemKey) -> Result<&crate::seam::SeamData, LazyRefusal> {
        let at = self.statics.values.binary_search_by_key(&item, |(x, _)| *x).map_err(|_| LazyRefusal::NoStatic { item })?;
        Ok(&self.statics.values[at].1)
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

    /// The tables of the principals a checked body's types (and the heads made so far) name: a
    /// prefetch only. What an instance's build reads is asked by name when it is missing
    /// ([`Self::object_asked`], the seam's `object_table`), so no make depends on which bodies
    /// were checked before it (D194: a `dyn` only an instance's ground arguments hold).
    fn body_objects(&mut self, checked: &Checked<'static>, function: &str) -> Result<(), LazyRefusal> {
        let asked = dyn_principals([self.store.tys.view(), checked.types.view()].into_iter());
        self.objects_made(asked, function)
    }

    /// The seam's `object_table`: trait `t`'s table and every supertrait's under it, made from
    /// their heads (program.rs `object_tables`, the one maker) when the build asked for it and
    /// found none. `Ok(false)` when it is there already: the build's `NoTrait` is then not a
    /// missing table, and stays the refusal.
    pub(super) fn object_asked(&mut self, t: ItemKey) -> Result<bool, LazyRefusal> {
        if self.objects.contains_key(&t) {
            return Ok(false);
        }
        let mut asked: Vec<ItemKey, Global> = Vec::new_in(Global);
        asked.push(t);
        let function = format!("the trait object table of {} (asked by an instance's build)", self.shown(t));
        self.objects_made(asked, &function)?;
        Ok(self.objects.contains_key(&t))
    }

    /// The seam's `site_heads` (D194): the head of each item enclosing body-local `owner`,
    /// innermost out, made from its own want, so a `#[track_caller]` site of `owner`'s body
    /// (seam_rules.rs `body_anchor`, which counts a written local's place from its enclosing
    /// item's text) never depends on whether this program checked the enclosing body before.
    /// A non-local `owner` asks nothing. Bounded by the locals kept (rule 18), as `body_anchor`.
    pub(super) fn site_heads_asked(&mut self, owner: ItemKey) -> Result<(), LazyRefusal> {
        let mut at = owner;
        for _ in 0..=self.store.locals.kept() {
            let Some(enclosing) = self.store.locals.head(at).map(|local| local.enclosing) else {
                return Ok(());
            };
            if !self.store.fns.contains_key(&enclosing) {
                let (_, want) = self.target_of(enclosing)?;
                self.make_want(want, enclosing, "site_heads (the enclosing item a body-local's `#[track_caller]` site counts from)")?;
            }
            at = enclosing;
        }
        Ok(())
    }

    /// Each of `asked` not made yet, made into the program's tables; `function` names the asker
    /// in a refusal.
    fn objects_made(&mut self, mut asked: Vec<ItemKey, Global>, function: &str) -> Result<(), LazyRefusal> {
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

    pub fn lowering_counts(&self) -> LoweringCounts {
        self.lowerings.count
    }

    pub(super) fn head_types(&self) -> PTys<'_> {
        #[cfg(test)]
        if let Some(tys) = &self.lowerings.refork {
            return tys.view();
        }
        self.store.tys.view()
    }

    /// The body owns its suffix of nodes and list entries. OwnTys::view truncates both
    /// head columns to their lengths at conversion, even after another check grew them.
    pub(super) fn types_of_body(&self, i: usize) -> PTys<'_> {
        #[cfg(test)]
        if let Some(tys) = &self.lowerings.refork {
            return tys.view();
        }
        let live = self.live_of(i).expect("body types require a live function row");
        // check_body already read this overlay over this very store. Both head columns
        // are append-only and the store is never replaced.
        live.made.tys.view(&self.store.tys).expect("a checked body's head prefix must still exist")
    }

    fn keep_body(&mut self, i: usize, made: Body<'n>, function: &str) -> Result<(), LazyRefusal> {
        #[cfg(test)]
        let made = if self.lowerings.refork.is_some() { self.refork(&made, function)? } else { made };
        let lits = literals(&made, function).map_err(|refusal| LazyRefusal::Row { function: function.into(), refusal: refusal.into() })?;
        let live = Live { row: i as u32, made, lits };
        let slot = if self.counts.live as usize == self.live.len() {
            self.live.push(Some(live));
            self.live.len() - 1
        } else {
            let free = self.live.iter().position(Option::is_none).expect("a released live slot");
            self.live[free] = Some(live);
            free
        };
        self.rows.live[i] = Some(slot as u32);
        self.counts.live += 1;
        Ok(())
    }

    /// The pre-overlay build path, selected on a fresh program by the byte-equality test.
    #[cfg(test)]
    pub(crate) fn use_refork_oracle(&mut self) {
        assert_eq!(self.lowerings.count, LoweringCounts::default());
        self.lowerings.refork = Some(PortableTys::new_in(self.store.tys.alloc()));
    }

    #[cfg(test)]
    fn refork(&mut self, made: &Body<'n>, function: &str) -> Result<Body<'n>, LazyRefusal> {
        self.lowerings.count.forks += 1;
        let mut tys = self.store.tys.fork_in(self.store.tys.alloc()).map_err(|_| LazyRefusal::Table { function: Box::from(function) })?;
        let mut again = |body: &Body<'n>| {
            self.lowerings.count.lower_again += 1;
            lower_again(self.front.names, self.prepared, &mut self.front.keys, &mut self.store, body, &mut tys)
                .map_err(|error| LazyRefusal::Lower { function: Box::from(function), error })
        };
        // lower_again identifies its input solely by owner, which both lowerings share.
        for live in self.live.iter_mut().flatten() {
            live.made = again(&live.made)?;
        }
        let body = again(made)?;
        self.lowerings.refork = Some(tys);
        Ok(body)
    }
}
