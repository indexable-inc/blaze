//! The flow-lower fixtures' region facts (D148, flow.md "Lifetimes are produced once and carried
//! beside the types"): each head a test needs is DECLARED by hand, its lifetime parameters and
//! its region column as the head lowering lays them, never read from a text. One shared builder
//! ([`Decls`]) for every test file of the crate (NEVER COMPILED when written, 2026-11-19, lane
//! LT1 of wave-1119).
//!
//! THE NUMBERING (blaze-typeck regions.rs `Numbering`, blaze-ladder front2 heads.rs): `Param(i)`
//! is the container's lifetimes first, then the item's own in DECLARATION order, then one per
//! elided input lifetime. A column's slots are in its type's pre-order, list children in order,
//! each node counted by `region_arity`: a reference and a trait object one own slot, an ADT its
//! lifetime count, an opaque type its captures, a projection its alias lifetimes, a clause's
//! trait (`ExTrait`) the trait's, a fn pointer and every other node none. A fn's column is laid
//! over `fn(inputs..) -> output`, whose root has no slot: child `k` is input `k`, the last child
//! the output. A lifetime a fn-pointer TYPE binds (`f: fn(&S)`) is `Late { binder, var }`,
//! `binder` the binding node's index among the binders of the laid type in pre-order: the
//! signature's root fn pointer is binder 0, so a fn's first fn-pointer parameter binds binder 1
//! (heads.rs `a_fn_pointer_argument_binds_its_own_lifetimes`). Late-bound
//! ([`FnHead::late`]): an own lifetime the inputs use and no bound names, and every elided
//! input lifetime (heads.rs, the end of the signature's lowering).
//!
//! THE TABLE: a column holds `PTy`s, and the lowering reads them in the CHECKED record's table
//! (`Checked::types`, lib.rs `sig_declared`: `self.node(sig.regions().col.ty(), ..)`), a fresh
//! table the type check exports, never the world's (`Fx::t`). So a head is declared over the
//! world's types and laid after the type check ([`Decls::lay`]): each declared type is imported
//! into the record's table first (interned by structure, so the record's own `&S` and the
//! imported one are one `PTy`), and the column is laid over the import.
//!
//! [`run`] is the one runner: check the body, lay the heads into its record, lower it under the
//! test's facts with these regions ([`WithRegions`]), and check the flow.

use std::boxed::Box;
use std::format;
use std::string::String;
use std::vec;

use blaze_flow::record::check;
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::region_facts::{ClauseRegions, ClauseView, ColView, HeadRegions, Outlives, ParamsView, RegionFacts, RegionsUnread, SigRegions, UnreadWhy};
use blaze_typeck::regions::{Arity, Count, LifetimeParams, Region, RegionCol};
use blaze_typeck::{BodyView, Checked, ExprId, FnPtrHeader, InferCtx, ItemKey, List, Name, Needs, Outcome, PNode, PTy, PTys, PortableTys};

use crate::{AdtFacts, CallBounds, CalleeFacts, Facts, FieldLead, FieldRegion, Plant, Refusal, Requirement, Shape, Stats, Ties, Variance};

/// The fixtures' allocator: the test heap.
pub(crate) fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// The variances of a fixture ADT whose `n` parameters (lifetimes first, as
/// `Facts::adt_variances` lists them) each appear only under a shared reference or by value in
/// its fields (`struct Hold<'a>(&'a S);`, a lifetime must be used: E0392): covariant in every
/// one (the Reference, "Subtyping and Variance": an ADT is as its fields are). A fixture with
/// another shape of field states its own.
pub(crate) fn all_co(n: u32) -> &'static [Variance] {
    const CO: &[Variance] = &[Variance::Co; 8];
    match CO.get(..n as usize) {
        Some(s) => s,
        None => panic!("a fixture ADT with {n} parameters: `all_co` states at most 8"),
    }
}

/// `'0`, `'1`, `'2` of a head's numbering.
pub(crate) const P0: Region = Region::Param(0);
pub(crate) const P1: Region = Region::Param(1);
pub(crate) const P2: Region = Region::Param(2);
/// The lifetime a fn's FIRST fn-pointer parameter binds (`f: fn(&S)`): binder 1 (the
/// signature's root is binder 0), its first variable.
pub(crate) const BOUND1: Region = Region::Late { binder: 1, var: 0 };

/// One fn's head as a test declares it: `lifetimes` parameters (container, own, elided, in that
/// order), which of them are late-bound, the declared `'long: 'short` among them, and the column
/// `slots` laid over `fn(inputs..) -> output` (types of the world's table).
#[derive(Clone, Copy, Debug)]
pub(crate) struct FnHead<'d> {
    pub(crate) lifetimes: u32,
    pub(crate) late: &'d [u32],
    pub(crate) outlives: &'d [(Region, Region)],
    pub(crate) inputs: &'d [PTy],
    pub(crate) output: PTy,
    pub(crate) slots: &'d [Region],
}

/// One impl's head as a test declares it: `lifetimes` parameters, the self type's column
/// `self_slots` over `self_ty`, and the trait reference (the trait, its arguments after `Self`,
/// and their column: the trait's lifetime arguments first, then each argument's slots) or `None`
/// for an inherent impl. [`Decls::lay`] lays it subject-first as production does
/// (`ExTrait(trait, [Self, args..])`, `self_slots` put after the trait's lifetimes).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ImplHead<'d> {
    pub(crate) lifetimes: u32,
    pub(crate) outlives: &'d [(Region, Region)],
    pub(crate) self_ty: PTy,
    pub(crate) self_slots: &'d [Region],
    pub(crate) trait_ref: Option<(ItemKey, &'d [PTy], &'d [Region])>,
}

struct FnDecl {
    item: ItemKey,
    lifetimes: u32,
    late: vec::Vec<u32>,
    outlives: vec::Vec<Outlives>,
    inputs: vec::Vec<PTy>,
    output: PTy,
    slots: vec::Vec<Region>,
    written: Option<(PTy, vec::Vec<Region>)>,
    clauses: vec::Vec<(u32, u32, PTy, vec::Vec<Region>)>,
}

struct ImplDecl {
    item: ItemKey,
    lifetimes: u32,
    outlives: vec::Vec<Outlives>,
    self_ty: PTy,
    self_slots: vec::Vec<Region>,
    trait_ref: Option<(ItemKey, vec::Vec<PTy>, vec::Vec<Region>)>,
}

/// The heads of one test, declared over the world's types; laid by [`Decls::lay`]. Also the
/// counts the one region counter reads ([`Arity`]): an ADT not declared has no lifetime (as
/// tests.rs `Plain`'s `AdtFacts::lifetimes`; a test whose `Facts::adt` says otherwise declares
/// it with [`Decls::adt`], the two must agree); an opaque type, a trait or an associated type
/// not declared is not known, and a column over it is refused by `RegionCol::lay` (the test
/// panics naming it).
#[derive(Default)]
pub(crate) struct Decls {
    fns: vec::Vec<FnDecl>,
    impls: vec::Vec<ImplDecl>,
    fields: vec::Vec<(ItemKey, u32, u32, PTy, vec::Vec<Region>)>,
    assocs: vec::Vec<(ItemKey, Name, PTy, vec::Vec<Region>)>,
    refused: vec::Vec<ItemKey>,
    adts: vec::Vec<(ItemKey, u32)>,
    opaques: vec::Vec<(ItemKey, u32)>,
    traits: vec::Vec<(ItemKey, u32)>,
    aliases: vec::Vec<(ItemKey, Name, u32)>,
    /// An opaque type's own head: `(opaque, lifetimes, nth, binders, ty, slots)`, one row per
    /// clause ([`Decls::opaque_clause`]).
    opaque_clauses: vec::Vec<(ItemKey, u32, u32, u32, PTy, vec::Vec<Region>)>,
    /// An opaque type's own head's `+ 'r` bounds: `(opaque, lifetimes, clause, region)`
    /// ([`Decls::opaque_outlives`]).
    opaque_outlives: vec::Vec<(ItemKey, u32, u32, Region)>,
}

fn region_outlives(pairs: &[(Region, Region)]) -> vec::Vec<Outlives> {
    pairs.iter().map(|&(long, short)| Outlives::Region { long, short }).collect()
}

impl Decls {
    pub(crate) fn new() -> Decls {
        Decls::default()
    }

    /// Fn `item`'s head.
    pub(crate) fn func(&mut self, item: ItemKey, head: FnHead<'_>) {
        self.fns.push(FnDecl {
            item,
            lifetimes: head.lifetimes,
            late: head.late.to_vec(),
            outlives: region_outlives(head.outlives),
            inputs: head.inputs.to_vec(),
            output: head.output,
            slots: head.slots.to_vec(),
            written: None,
            clauses: vec::Vec::new(),
        });
    }

    fn fn_mut(&mut self, item: ItemKey) -> &mut FnDecl {
        match self.fns.iter_mut().find(|f| f.item == item) {
            Some(f) => f,
            None => panic!("{item:?} has no declared head"),
        }
    }

    /// An async fn `item`'s WRITTEN return type `ty` and its column (the future's output).
    pub(crate) fn written_output(&mut self, item: ItemKey, ty: PTy, slots: &[Region]) {
        self.fn_mut(item).written = Some((ty, slots.to_vec()));
    }

    /// Clause `nth` of fn `item` (`binders` its own `for<..>` binds), its column `slots` over
    /// `ty` (a type of the world's table: the clause's `ExTrait` or its subject).
    pub(crate) fn clause(&mut self, item: ItemKey, nth: u32, binders: u32, ty: PTy, slots: &[Region]) {
        self.fn_mut(item).clauses.push((nth, binders, ty, slots.to_vec()));
    }

    /// Fn `item`'s declared `T: 'region` (its clause `clause`).
    pub(crate) fn type_outlives(&mut self, item: ItemKey, clause: u32, region: Region) {
        self.fn_mut(item).outlives.push(Outlives::Type { clause, region });
    }

    /// Impl `item`'s head.
    pub(crate) fn imp(&mut self, item: ItemKey, head: ImplHead<'_>) {
        self.impls.push(ImplDecl {
            item,
            lifetimes: head.lifetimes,
            outlives: region_outlives(head.outlives),
            self_ty: head.self_ty,
            self_slots: head.self_slots.to_vec(),
            trait_ref: head.trait_ref.map(|(t, args, slots)| (t, args.to_vec(), slots.to_vec())),
        });
    }

    /// Field `field` of `variant` of ADT `adt`: its declared type `ty` and the column over it,
    /// in the ADT's numbering (its lifetimes, [`Decls::adt`]).
    pub(crate) fn field(&mut self, adt: ItemKey, variant: u32, field: u32, ty: PTy, slots: &[Region]) {
        self.fields.push((adt, variant, field, ty, slots.to_vec()));
    }

    /// Impl `imp`'s `type name = ty;` and the column over `ty` (a type of the world's table), in
    /// the impl's numbering (`RegionFacts::impl_assoc`).
    pub(crate) fn impl_assoc(&mut self, imp: ItemKey, name: &str, ty: PTy, slots: &[Region]) {
        self.assocs.push((imp, Name::of(name), ty, slots.to_vec()));
    }

    /// `item`'s head was refused (E0106 is one): its regions are `HeadRefused`.
    pub(crate) fn refuse(&mut self, item: ItemKey) {
        self.refused.push(item);
    }

    /// ADT `adt` declares `n` lifetimes.
    pub(crate) fn adt(&mut self, adt: ItemKey, n: u32) {
        self.adts.push((adt, n));
    }

    /// Opaque type `opaque` captures `n` lifetimes.
    pub(crate) fn opaque(&mut self, opaque: ItemKey, n: u32) {
        self.opaques.push((opaque, n));
    }

    /// Clause `nth` of opaque type `opaque`'s own head, as blaze-ladder heads.rs `opaque_filed`
    /// files it: in its fn's numbering (`lifetimes` the fn's count), its column `slots` over
    /// `ty` (the clause's `ExTrait` or `ExProj`, `Self` the opaque).
    pub(crate) fn opaque_clause(&mut self, opaque: ItemKey, lifetimes: u32, nth: u32, binders: u32, ty: PTy, slots: &[Region]) {
        self.opaque_clauses.push((opaque, lifetimes, nth, binders, ty, slots.to_vec()));
    }

    /// Opaque type `opaque`'s bound `+ region` (its clause `clause`), in its fn's numbering
    /// (`lifetimes` the fn's count): an `Outlives::Type` of its own head, as blaze-ladder
    /// heads_regions.rs `opaque_regions` files `impl Sized + 'static`.
    pub(crate) fn opaque_outlives(&mut self, opaque: ItemKey, lifetimes: u32, clause: u32, region: Region) {
        self.opaque_outlives.push((opaque, lifetimes, clause, region));
    }

    /// Trait `trait_` declares `n` lifetimes (declared again, the same `n`: one declaration).
    pub(crate) fn trait_(&mut self, trait_: ItemKey, n: u32) {
        match self.traits.iter().find(|r| r.0 == trait_) {
            Some(&(_, m)) if m == n => {}
            Some(&(_, m)) => panic!("{trait_:?} declared with {m} lifetimes and with {n}"),
            None => self.traits.push((trait_, n)),
        }
    }

    /// `<_ as trait_>::name` has `n` lifetimes (the trait's, then a GAT's own).
    pub(crate) fn alias(&mut self, trait_: ItemKey, name: &str, n: u32) {
        self.aliases.push((trait_, Name::of(name), n));
    }

    /// Every declared head laid into `into` (the checked record's table, `Checked::types`), its
    /// types imported from `from` (the world's, `Fx::t`). A column that does not fit its type is
    /// a broken fixture: it panics naming the item.
    pub(crate) fn lay(&self, from: PTys<'_>, into: &mut PortableTys<'_>) -> Regions {
        let a = alloc();
        let mut heads: vec::Vec<(ItemKey, &'static HeadRegions<Alloc<'static>>, &'static [ClauseView<'static>])> = vec::Vec::new();
        let params_of = move |item: ItemKey, count: u32, late: &[u32], outlives: &[Outlives]| {
            let mut params = LifetimeParams::new_in(count, a);
            for &i in late {
                params.set_late(i).unwrap_or_else(|e| panic!("{item:?}'s late lifetime {i}: {e:?}"));
            }
            for o in outlives {
                if let Outlives::Region { long, short } = *o {
                    params.add_outlives(long, short).unwrap_or_else(|e| panic!("{item:?}'s outlives {o:?}: {e:?}"));
                }
            }
            params
        };
        for f in self.fns.iter() {
            let mut io: vec::Vec<PTy> = f.inputs.iter().map(|&t| import(from, t, into)).collect();
            io.push(import(from, f.output, into));
            let io = into.intern_list(&io).unwrap_or_else(|e| panic!("{:?}'s inputs: {e:?}", f.item));
            let root = into.intern(PNode::FnPtr(io, FnPtrHeader::RUST)).unwrap_or_else(|e| panic!("{:?}'s signature: {e:?}", f.item));
            let mut head = HeadRegions::new_in(params_of(f.item, f.lifetimes, &f.late, &f.outlives), a);
            head.outlives.extend_from_slice(&f.outlives);
            head.sig = Some(self.col(into, root, &f.slots, f.item));
            if let Some((ty, slots)) = &f.written {
                let t = import(from, *ty, into);
                head.written_output = Some(self.col(into, t, slots, f.item));
            }
            for (nth, binders, ty, slots) in f.clauses.iter() {
                let t = import(from, *ty, into);
                head.clauses.push(ClauseRegions { nth: *nth, binders: *binders, col: self.col(into, t, slots, f.item) });
            }
            heads.push(leak(f.item, head));
        }
        for i in self.impls.iter() {
            let mut head = HeadRegions::new_in(params_of(i.item, i.lifetimes, &[], &i.outlives), a);
            head.outlives.extend_from_slice(&i.outlives);
            let self_ty = import(from, i.self_ty, into);
            let self_col = self.col(into, self_ty, &i.self_slots, i.item);
            // SUBJECT-FIRST, as production lays a header (blaze-ladder heads_regions.rs
            // `header_trait`): `ExTrait(trait, [Self, args..])` over the trait's lifetime
            // arguments, then `Self`'s slots (the self column's), then each argument's. The
            // declaration names the arguments after `Self` and their slots; `Self` is put in here.
            let trait_col = i.trait_ref.as_ref().map(|(t, args, slots)| {
                let own = match self.trait_lifetimes(*t) {
                    Count::Known(n) => n as usize,
                    Count::NotKnown => panic!("{:?}'s trait {t:?} is not declared (`Decls::trait_`)", i.item),
                };
                let mut all: vec::Vec<PTy> = vec::Vec::new();
                all.push(self_ty);
                all.extend(args.iter().map(|&x| import(from, x, into)));
                let all = into.intern_list(&all).unwrap_or_else(|e| panic!("{:?}'s trait arguments: {e:?}", i.item));
                let tr = into.intern(PNode::ExTrait(*t, all)).unwrap_or_else(|e| panic!("{:?}'s trait reference: {e:?}", i.item));
                let Some((theirs, rest)) = slots.split_at_checked(own) else {
                    panic!("{:?}'s trait column {slots:?} is shorter than the trait's {own} lifetimes", i.item)
                };
                let mut col: vec::Vec<Region> = theirs.to_vec();
                col.extend_from_slice(&i.self_slots);
                col.extend_from_slice(rest);
                self.col(into, tr, &col, i.item)
            });
            head.impl_header = Some((self_col, trait_col));
            heads.push(leak(i.item, head));
        }
        // An opaque type with a declared clause or bound: one head per opaque, in its fn's
        // numbering.
        let mut opaques: vec::Vec<(ItemKey, u32)> = vec::Vec::new();
        let declared = self.opaque_clauses.iter().map(|r| (r.0, r.1)).chain(self.opaque_outlives.iter().map(|r| (r.0, r.1)));
        for (opaque, lifetimes) in declared {
            if !opaques.iter().any(|r| r.0 == opaque) {
                opaques.push((opaque, lifetimes));
            }
        }
        for (opaque, lifetimes) in opaques {
            let mut head = HeadRegions::new_in(params_of(opaque, lifetimes, &[], &[]), a);
            for (_, _, nth, binders, ty, slots) in self.opaque_clauses.iter().filter(|r| r.0 == opaque) {
                let t = import(from, *ty, into);
                head.clauses.push(ClauseRegions { nth: *nth, binders: *binders, col: self.col(into, t, slots, opaque) });
            }
            for &(_, _, clause, region) in self.opaque_outlives.iter().filter(|r| r.0 == opaque) {
                head.outlives.push(Outlives::Type { clause, region });
            }
            heads.push(leak(opaque, head));
        }
        // An ADT with a declared field: one head per ADT, its parameters the ADT's lifetimes.
        let mut adts: vec::Vec<ItemKey> = vec::Vec::new();
        for &(adt, ..) in self.fields.iter() {
            if !adts.contains(&adt) {
                adts.push(adt);
            }
        }
        for adt in adts {
            let count = match self.adt_lifetimes(adt) {
                Count::Known(n) => n,
                Count::NotKnown => panic!("{adt:?}'s lifetimes are not declared"),
            };
            let mut head = HeadRegions::new_in(params_of(adt, count, &[], &[]), a);
            for (_, variant, field, ty, slots) in self.fields.iter().filter(|r| r.0 == adt) {
                let t = import(from, *ty, into);
                head.fields.push((*variant, *field, self.col(into, t, slots, adt)));
            }
            heads.push(leak(adt, head));
        }
        let mut assocs: vec::Vec<(ItemKey, Name, &'static RegionCol<Alloc<'static>>)> = vec::Vec::new();
        for (imp, name, ty, slots) in self.assocs.iter() {
            let t = import(from, *ty, into);
            let col: &'static RegionCol<Alloc<'static>> = Box::leak(Box::new(self.col(into, t, slots, *imp)));
            assocs.push((*imp, *name, col));
        }
        Regions { heads, assocs, refused: self.refused.clone(), opaques: self.opaques.clone(), traits: self.traits.clone(), aliases: self.aliases.clone() }
    }

    fn col(&self, into: &PortableTys<'_>, ty: PTy, slots: &[Region], item: ItemKey) -> RegionCol<Alloc<'static>> {
        RegionCol::lay(into.view(), ty, self, slots, alloc()).unwrap_or_else(|e| panic!("{item:?}'s column {slots:?} does not fit its type: {e:?}"))
    }
}

impl Arity for Decls {
    fn adt_lifetimes(&self, adt: ItemKey) -> Count {
        Count::Known(self.adts.iter().find(|r| r.0 == adt).map_or(0, |r| r.1))
    }
    fn opaque_captures(&self, opaque: ItemKey) -> Count {
        self.opaques.iter().find(|r| r.0 == opaque).map_or(Count::NotKnown, |r| Count::Known(r.1))
    }
    fn alias_lifetimes(&self, trait_: ItemKey, name: Name) -> Count {
        self.aliases.iter().find(|r| r.0 == trait_ && r.1 == name).map_or(Count::NotKnown, |r| Count::Known(r.2))
    }
    fn trait_lifetimes(&self, trait_: ItemKey) -> Count {
        self.traits.iter().find(|r| r.0 == trait_).map_or(Count::NotKnown, |r| Count::Known(r.1))
    }
}

/// A laid head kept for the rest of the test process: its clause views borrow its columns, so
/// both live as long as the views handed out (`RegionFacts::clauses` returns a slice of views).
fn leak(item: ItemKey, head: HeadRegions<Alloc<'static>>) -> (ItemKey, &'static HeadRegions<Alloc<'static>>, &'static [ClauseView<'static>]) {
    let head: &'static HeadRegions<Alloc<'static>> = Box::leak(Box::new(head));
    let views: vec::Vec<ClauseView<'static>> = head.clauses.iter().map(ClauseView::of).collect();
    (item, head, Box::leak(views.into_boxed_slice()))
}

/// `t` of the table `from`, interned in `into` node by node (children first, as a table holds
/// them): the same type in the other table.
fn import(from: PTys<'_>, t: PTy, into: &mut PortableTys<'_>) -> PTy {
    let node = from.node(t).unwrap_or_else(|e| panic!("{t:?} is no type of the world's table: {e:?}"));
    let n = match node {
        PNode::Bool
        | PNode::Char
        | PNode::Int(_)
        | PNode::Uint(_)
        | PNode::Float(_)
        | PNode::Str
        | PNode::Never
        | PNode::Param(_)
        | PNode::Infer
        | PNode::Error
        | PNode::ConstParam(_)
        | PNode::ExAuto(_)
        | PNode::Foreign(_) => node,
        PNode::Tuple(l) => PNode::Tuple(import_list(from, l, into)),
        PNode::Record(s, l) => PNode::Record(s, import_list(from, l, into)),
        PNode::Union(l) => PNode::Union(import_list(from, l, into)),
        PNode::Ref(m, c) => PNode::Ref(m, import(from, c, into)),
        PNode::Ptr(m, c) => PNode::Ptr(m, import(from, c, into)),
        PNode::Array(elem, len) => {
            let elem = import(from, elem, into);
            PNode::Array(elem, import(from, len, into))
        }
        PNode::Slice(c) => PNode::Slice(import(from, c, into)),
        PNode::Pat(base, ranges) => {
            let base = import(from, base, into);
            PNode::Pat(base, import_list(from, ranges, into))
        }
        PNode::Adt(k, l) => PNode::Adt(k, import_list(from, l, into)),
        PNode::FnDef(k, l) => PNode::FnDef(k, import_list(from, l, into)),
        PNode::FnPtr(l, h) => PNode::FnPtr(import_list(from, l, into), h),
        PNode::Closure { owner, ordinal, args } => PNode::Closure { owner, ordinal, args: import_list(from, args, into) },
        PNode::Future(e, output, interior) => {
            let output = import(from, output, into);
            PNode::Future(e, output, import(from, interior, into))
        }
        PNode::Coroutine { owner, ordinal, args } => PNode::Coroutine { owner, ordinal, args: import_list(from, args, into) },
        PNode::Alias { trait_, name, args } => PNode::Alias { trait_, name, args: import_list(from, args, into) },
        PNode::Opaque(k, l) => PNode::Opaque(k, import_list(from, l, into)),
        PNode::ConstValue { ty, bits } => PNode::ConstValue { ty: import(from, ty, into), bits },
        // The predicates as the world's table holds them: already in `dynamic`'s one form.
        PNode::Dynamic(l) => PNode::Dynamic(import_list(from, l, into)),
        PNode::ExTrait(k, l) => PNode::ExTrait(k, import_list(from, l, into)),
        PNode::ExProj { trait_, name, args, term } => {
            let args = import_list(from, args, into);
            PNode::ExProj { trait_, name, args, term: import(from, term, into) }
        }
    };
    into.intern(n).unwrap_or_else(|e| panic!("{n:?} was not interned: {e:?}"))
}

fn import_list(from: PTys<'_>, l: List, into: &mut PortableTys<'_>) -> List {
    let kids = from.list(l).unwrap_or_else(|e| panic!("{l:?} is no list of the world's table: {e:?}"));
    let kids: vec::Vec<PTy> = kids.iter().map(|&c| import(from, c, into)).collect();
    into.intern_list(&kids).unwrap_or_else(|e| panic!("{kids:?} was not interned: {e:?}"))
}

/// The laid heads of one test ([`Decls::lay`]): what `Facts::regions` answers under [`run`].
pub(crate) struct Regions {
    heads: vec::Vec<(ItemKey, &'static HeadRegions<Alloc<'static>>, &'static [ClauseView<'static>])>,
    assocs: vec::Vec<(ItemKey, Name, &'static RegionCol<Alloc<'static>>)>,
    refused: vec::Vec<ItemKey>,
    opaques: vec::Vec<(ItemKey, u32)>,
    /// The declared counts the one call rule reads ([`Facts::trait_lifetimes`],
    /// [`Facts::alias_lifetimes`]): the same the columns were laid by.
    traits: vec::Vec<(ItemKey, u32)>,
    aliases: vec::Vec<(ItemKey, Name, u32)>,
}

impl Regions {
    fn head(&self, item: ItemKey) -> Result<(&HeadRegions<Alloc<'static>>, &[ClauseView<'static>]), RegionsUnread> {
        if self.refused.contains(&item) {
            return Err(RegionsUnread { item, why: UnreadWhy::HeadRefused });
        }
        match self.heads.iter().find(|r| r.0 == item) {
            Some(&(_, head, clauses)) => Ok((head, clauses)),
            None => Err(RegionsUnread { item, why: UnreadWhy::NoHead }),
        }
    }
}

impl RegionFacts for Regions {
    fn sig(&self, item: ItemKey) -> Result<SigRegions<'_>, RegionsUnread> {
        let (head, _) = self.head(item)?;
        head.sig_view().ok_or(RegionsUnread { item, why: UnreadWhy::NoHead })
    }
    fn clauses(&self, item: ItemKey) -> Result<&[ClauseView<'_>], RegionsUnread> {
        Ok(self.head(item)?.1)
    }
    fn outlives(&self, item: ItemKey) -> Result<&[Outlives], RegionsUnread> {
        Ok(&self.head(item)?.0.outlives)
    }
    fn field(&self, adt: ItemKey, variant: u32, field: u32) -> Result<ColView<'_>, RegionsUnread> {
        let (head, _) = self.head(adt)?;
        let found = head.fields.iter().find(|(v, f, _)| *v == variant && *f == field);
        found.map(|(_, _, col)| ColView::of(col)).ok_or(RegionsUnread { item: adt, why: UnreadWhy::NoHead })
    }
    fn impl_header(&self, impl_: ItemKey) -> Result<(ColView<'_>, Option<ColView<'_>>), RegionsUnread> {
        let (head, _) = self.head(impl_)?;
        let header = head.impl_header.as_ref().map(|(s, t)| (ColView::of(s), t.as_ref().map(ColView::of)));
        header.ok_or(RegionsUnread { item: impl_, why: UnreadWhy::NoHead })
    }
    fn opaque_captures(&self, opaque: ItemKey) -> Result<u32, RegionsUnread> {
        self.opaques.iter().find(|r| r.0 == opaque).map(|r| r.1).ok_or(RegionsUnread { item: opaque, why: UnreadWhy::NoHead })
    }
    fn lifetime_params(&self, item: ItemKey) -> Result<ParamsView<'_>, RegionsUnread> {
        Ok(ParamsView::of(&self.head(item)?.0.params))
    }
    fn impl_assoc(&self, impl_: ItemKey, name: Name) -> Result<ColView<'_>, RegionsUnread> {
        self.head(impl_)?;
        let found = self.assocs.iter().find(|r| r.0 == impl_ && r.1 == name);
        found.map(|r| ColView::of(r.2)).ok_or(RegionsUnread { item: impl_, why: UnreadWhy::NoHead })
    }
}

/// A test's facts with the laid [`Regions`]: every fact the test's own answers, the regions
/// these. `Facts::sig` is not answered here: its one definition reads `regions`.
pub(crate) struct WithRegions<'f, F: Facts> {
    pub(crate) base: &'f F,
    pub(crate) regions: &'f Regions,
}

impl<F: Facts> Facts for WithRegions<'_, F> {
    // Every other fact not known here, written out: `Facts` has no default answer.
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt(&self, adt: ItemKey) -> Option<AdtFacts> {
        self.base.adt(adt)
    }
    fn callee(&self, item: ItemKey) -> Option<CalleeFacts> {
        self.base.callee(item)
    }
    fn regions(&self) -> &dyn RegionFacts {
        self.regions
    }
    fn adt_variances(&self, adt: ItemKey) -> Option<&[Variance]> {
        self.base.adt_variances(adt)
    }
    fn fn_binders(&self) -> bool {
        self.base.fn_binders()
    }
    fn closure_bodies_checked(&self) -> bool {
        self.base.closure_bodies_checked()
    }
    fn closure_requirements(&self, closure: ExprId) -> &[Requirement] {
        self.base.closure_requirements(closure)
    }
    fn async_bodies_checked(&self) -> bool {
        self.base.async_bodies_checked()
    }
    fn is_future(&self, ty: PTy) -> Option<bool> {
        self.base.is_future(ty)
    }
    fn shape(&self, item: ItemKey) -> Option<Shape<'_>> {
        self.base.shape(item)
    }
    fn object_trait_lifetimes(&self, trait_: ItemKey) -> Option<bool> {
        self.base.object_trait_lifetimes(trait_)
    }
    fn call_bounds(&self, item: ItemKey) -> Option<CallBounds<'_>> {
        self.base.call_bounds(item)
    }
    fn is_fn_trait(&self, trait_: ItemKey) -> bool {
        self.base.is_fn_trait(trait_)
    }
    fn deref_impl(&self, adt: ItemKey) -> Option<ItemKey> {
        self.base.deref_impl(adt)
    }
    fn is_zero_slot_builtin_assoc(&self, trait_: ItemKey, name: Name) -> bool {
        self.base.is_zero_slot_builtin_assoc(trait_, name)
    }
    fn ties(&self, item: ItemKey) -> Option<Ties<'_>> {
        self.base.ties(item)
    }
    fn field(&self, adt: ItemKey, variant: u32, name: Name) -> Option<u32> {
        self.base.field(adt, variant, name)
    }
    fn field_param(&self, adt: ItemKey, variant: u32, field: u32) -> Option<u32> {
        self.base.field_param(adt, variant, field)
    }
    fn field_lead(&self, adt: ItemKey, variant: u32, field: u32) -> Option<FieldLead> {
        self.base.field_lead(adt, variant, field)
    }
    fn field_regions(&self, adt: ItemKey, variant: u32, field: u32) -> Option<&[FieldRegion]> {
        self.base.field_regions(adt, variant, field)
    }
    fn copy_args(&self, adt: ItemKey) -> Option<&[bool]> {
        self.base.copy_args(adt)
    }
    fn drop_args(&self, adt: ItemKey) -> Option<&[bool]> {
        self.base.drop_args(adt)
    }
    fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32> {
        self.base.field_count(adt, variant)
    }
    fn param_copy(&self, index: u32) -> bool {
        self.base.param_copy(index)
    }
    // Forwarded: a wrapper that answers a default here hides the base's answer (temp_scope_tests
    // `a_constant_initializers_temporary_is_static` set `constant_initializer` and the lowering
    // never saw it; blaze-ladder borrows_nested.rs forwards both for the same reason).
    fn constant_initializer(&self) -> bool {
        self.base.constant_initializer()
    }
    fn implied_bounds(&self) -> Option<(&[(u32, Region)], &[(Region, Region)])> {
        self.base.implied_bounds()
    }
    fn trait_lifetimes(&self, trait_: ItemKey) -> Option<u32> {
        match self.regions.traits.iter().find(|r| r.0 == trait_) {
            Some(r) => Some(r.1),
            None => self.base.trait_lifetimes(trait_),
        }
    }
    fn alias_lifetimes(&self, trait_: ItemKey, name: Name) -> Option<u32> {
        match self.regions.aliases.iter().find(|r| r.0 == trait_ && r.1 == name) {
            Some(r) => Some(r.2),
            None => self.base.alias_lifetimes(trait_, name),
        }
    }
    fn trait_relates_nothing(&self, trait_: ItemKey) -> bool {
        self.base.trait_relates_nothing(trait_)
    }
}

/// What one run gives a test: the findings, one line each (`move <kind> at <place> span
/// <lo>..<hi>`, `borrow <finding>`), the lowering's counts and the flow's text.
#[derive(Debug)]
pub(crate) struct Run {
    pub(crate) lines: vec::Vec<String>,
    pub(crate) stats: Stats,
    pub(crate) flow: String,
}

/// `view` checked by the real type checker, which must find it clean.
pub(crate) fn checked(fx: &Fx, view: &BodyView<'_>) -> Checked<'static> {
    let a = alloc();
    let mut cx = InferCtx::new_in(a);
    match blaze_typeck::check(view, fx, &mut cx, &Needs::new_in(a), a) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "the fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    }
}

/// `view`, checked as `c`, lowered under `facts` with `decls` laid into `c`'s table, and its flow
/// checked; or the lowering's refusal.
pub(crate) fn lower<F: Facts>(fx: &Fx, view: &BodyView<'_>, c: &mut Checked<'static>, facts: &F, decls: &Decls, plant: Plant) -> Result<Run, Refusal> {
    let regions = decls.lay(fx.t.view(), &mut c.types);
    let with = WithRegions { base: facts, regions: &regions };
    // Through the call-rule oracle (oracle.rs): both rules, compared per call, production's flow.
    let lowered = crate::oracle::lower_checked(view, c, &with, plant, alloc())?;
    let mut flow = String::new();
    lowered.flow.print(&mut flow).expect("print");
    let rec = check(&lowered.flow, alloc()).expect("bounded");
    // STRICT RELATIONS REFUSE ONLY ON A FINDING (`Lowered::coarse`), as blaze-ladder's
    // borrows.rs applies it: a region finding (any borrow finding; a move reads no region)
    // under a stricter relation refuses the body by that relation's name.
    if let Some(coarse) = lowered.coarse
        && !rec.borrows.is_empty()
    {
        return Err(coarse);
    }
    let mut lines = vec::Vec::new();
    for m in rec.moves.iter() {
        lines.push(format!("move {:?} at {:?} span {}..{}", m.kind, m.at, m.span.lo, m.span.hi));
    }
    for f in rec.borrows.iter() {
        lines.push(format!("borrow {f:?}"));
    }
    Ok(Run { lines, stats: lowered.stats, flow })
}

/// `view` checked, lowered under `facts` and `decls`' heads, and its flow checked ([`lower`]).
pub(crate) fn run<F: Facts>(fx: &Fx, view: &BodyView<'_>, facts: &F, decls: &Decls, plant: Plant) -> Result<Run, Refusal> {
    let mut c = checked(fx, view);
    lower(fx, view, &mut c, facts, decls, plant)
}

/// The findings of `root`, the body of `owner`, under `facts` and `decls`' heads ([`run`]).
pub(crate) fn findings<F: Facts>(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &F, decls: &Decls, plant: Plant) -> Result<vec::Vec<String>, Refusal> {
    run(fx, &b.view(owner, root), facts, decls, plant).map(|r| r.lines)
}

/// The fixture itself, each part with a planted control (rule 23).
mod checks {
    use blaze_typeck::Mutbl;

    use super::*;

    /// `fn pick<'a, 'b>(x: &'a S, y: &'b S) -> &'a S`: its column is laid in the CHECKED table
    /// and reads back as declared, the root's children over the record's own `&S`; its
    /// lifetimes as declared (both late). CONTROL: a column one slot short is refused by the
    /// lay (`RegionCol::lay`'s checked length), never laid.
    #[test]
    fn a_declared_head_is_laid_in_the_record_table() {
        let mut fx = Fx::new();
        let s = fx.strukt("S", 0, &[]);
        let st = fx.adt(s, &[]);
        let rs = fx.r(Mutbl::Not, st);
        let pick = fx.func("pick", 0, &[rs, rs], rs, vec::Vec::new());
        fx.finish();
        let mut decls = Decls::new();
        decls.func(pick, FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[rs, rs], output: rs, slots: &[P0, P1, P0] });
        let mut into = PortableTys::new_in(alloc());
        // A node of the record's own before the import: the two tables' numberings differ.
        let _ = into.intern(PNode::Bool);
        let regions = decls.lay(fx.t.view(), &mut into);
        let sig = regions.sig(pick).expect("laid");
        assert_eq!(sig.col.as_slice(), &[P0, P1, P0]);
        assert_eq!((sig.params.count(), sig.params.is_late(0).ok(), sig.params.is_late(1).ok()), (2, Some(true), Some(true)));
        let PNode::FnPtr(l, _) = into.view().node(sig.col.ty()).expect("in the record's table") else { panic!("not a signature") };
        let kids = into.view().list(l).expect("a list");
        assert!(matches!(into.view().node(kids[0]), Ok(PNode::Ref(Mutbl::Not, _))), "{kids:?}");
        // An item with no declared head: `NoHead`, never a guess.
        assert_eq!(regions.sig(s).map(|r| r.col.len()), Err(RegionsUnread { item: s, why: UnreadWhy::NoHead }));
        std::println!("a_declared_head_is_laid_in_the_record_table: {:?}", sig.col);
        let mut short = Decls::new();
        short.func(pick, FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[rs, rs], output: rs, slots: &[P0, P1] });
        let mut into = PortableTys::new_in(alloc());
        let control = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| short.lay(fx.t.view(), &mut into).heads.len()));
        assert!(control.is_err(), "CONTROL: a column one slot short was laid");
    }

    /// A refused head answers `HeadRefused`, an undeclared one `NoHead`. CONTROL: the same item
    /// declared and not refused answers its column.
    #[test]
    fn a_refused_head_is_refused_by_name() {
        let mut fx = Fx::new();
        let s = fx.strukt("S", 0, &[]);
        let st = fx.adt(s, &[]);
        let rs = fx.r(Mutbl::Not, st);
        let u8_ = fx.u8();
        let mk_ref = fx.func("mk_ref", 0, &[u8_], rs, vec::Vec::new());
        let other = fx.func("other", 0, &[u8_], u8_, vec::Vec::new());
        fx.finish();
        let mut decls = Decls::new();
        decls.refuse(mk_ref);
        let regions = decls.lay(fx.t.view(), &mut PortableTys::new_in(alloc()));
        assert_eq!(regions.sig(mk_ref).map(|s| s.col.len()), Err(RegionsUnread { item: mk_ref, why: UnreadWhy::HeadRefused }));
        assert_eq!(regions.sig(other).map(|s| s.col.len()), Err(RegionsUnread { item: other, why: UnreadWhy::NoHead }));
        let mut control = Decls::new();
        control.func(mk_ref, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[u8_], output: rs, slots: &[P0] });
        let regions = control.lay(fx.t.view(), &mut PortableTys::new_in(alloc()));
        assert_eq!(regions.sig(mk_ref).map(|s| s.col.len()), Ok(1), "CONTROL: a declared head is laid");
        std::println!("a_refused_head_is_refused_by_name: ok");
    }
}
