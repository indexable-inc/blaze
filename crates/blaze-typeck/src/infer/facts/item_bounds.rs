//! An associated type's item bounds, elaborated: each written bound with the supertraits of its
//! trait, transitively.
//!
//! `trait Seq { type Error: Err2; }` with `trait Err2: Sized + Std` says, for every `A: Seq`,
//! that `<A as Seq>::Error` is an `Err2`, and so `Sized` and a `Std` as well. The Reference:
//!
//! - "Associated Items", "Associated Types": "The optional trait bounds must be fulfilled by the
//!   implementations of the type alias."
//! - "Traits", "Supertraits": "Supertraits are traits that are required to be implemented for a
//!   type to implement a specific trait. Furthermore, anywhere a generic or trait object is
//!   bounded by a trait, it has access to the associated items of its supertraits." and
//!   "Supertraits are declared by trait bounds on the `Self` type of a trait and transitively
//!   the supertraits of the traits declared in those trait bounds."
//!
//! The solver assembles a rigid alias's bounds from `TraitDef::item_bounds` and an opaque
//! type's from `OpaqueDef::bounds` as they are given (blaze-solve alias.rs
//! `push_alias_bounds`), and elaborates only a param env (eval.rs `env`): so the facts given
//! here are the elaborated list, as a param env's clauses are, for both alias kinds
//! (facts.rs `fill_trait` for associated types, `fill_opaque` for opaque types: `impl
//! FnMut(A) -> B` is filed with a `FnOnce` projection, and `FnMut<Args>: FnOnce<Args>` is
//! what makes it a `FnOnce`; suite/opaque_supertraits.rs). Without it a
//! call of a trait's fn by the trait's path, whose predicates are the trait's own clause and
//! its supertrait clauses (`Self: Err2`, `Self: Sized`, `Self: Std`), is refused at
//! `<A as Seq>::Error: Std`: tempo's "E0277: Error" (serde's `Deserialize` derive:
//! `_serde::de::Error::invalid_length(..)` where `SeqAccess::Error: de::Error` and
//! `de::Error: Sized + std::error::Error`).
//!
//! A supertrait clause is written over its trait's parameters (`Self` is `Param(0)`), in that
//! trait's own table; it is converted here with each parameter replaced by the bound's
//! argument at that index, which is already a solver term.

use blaze_own::Vec;
use blaze_solve::term::{AliasKind as SAlias, Def as SDef, List as SList, Polarity, Pred, Rigid, Ty as STy, TyKind as K};

use super::{Conv, FactCache, assoc_def, digest};
use crate::error::{CheckError, Limit, Malformed, Stop};
use crate::ids::ItemKey;
use crate::portable::{PNode, PTy};
use crate::world::{PClause, Question, Reads, World};

/// The most bounds one associated type's elaborated list holds (rule 18).
const MAX_ELABORATED: usize = 4096;

/// The memo table number of a supertrait clause being instantiated: no record table has it
/// (`FactCache::table` numbers them from 0 by count), and its entries are removed when the
/// instance is done, since another instance of the same table has other arguments.
const INSTANCE_TAB: u32 = u32::MAX;

impl<'a> FactCache<'a> {
    /// The item bounds at `self.preds[start..]` of the synthetic associated type `assoc` whose
    /// item is the trait fn's opaque `opaque` (front2 rpitit.rs: an `impl Trait` of a trait fn,
    /// `<Self as Tr<..>>::{o}`), each with its `Self` made the projection: the opaque's
    /// predicates are written with `Self` the opaque `Opaque(o, X)`, which is by definition
    /// `<X0 as Tr<..>>::{o}` at the same arguments (its hidden type, rpitit.rs `Synthetic::alias`).
    /// RFC 3425 ("Return position `impl Trait` in traits"): the associated type's bounds are the
    /// `impl Trait`'s, so a rigid `<S as Tr>::{o}` (a `where S: Tr` in the env selects it, as
    /// tempo's `BasicEngineValidatorBuilder<..>: EngineValidatorBuilder<Node>`) is a `Future`
    /// with the written `Output` and `Send`. Kept with `Self` the opaque, the solver read them
    /// as no self bound of the projection (blaze-solve alias.rs `push_alias_bounds`) and
    /// answered "not a future" (a false E0277). A bound whose `Self` is anything else is kept.
    pub(super) fn opaque_self_as_projection(&mut self, start: usize, opaque: ItemKey, assoc: SDef) -> Result<(), Stop> {
        for i in start..self.preds.len() {
            match self.preds[i] {
                Pred::Trait { trait_, args, polarity } => {
                    if let Some(args) = self.self_swapped(args, opaque, assoc)? {
                        self.preds[i] = Pred::Trait { trait_, args, polarity };
                    }
                }
                Pred::HostEffect { trait_, args, constness } => {
                    if let Some(args) = self.self_swapped(args, opaque, assoc)? {
                        self.preds[i] = Pred::HostEffect { trait_, args, constness };
                    }
                }
                Pred::Projection { alias, term } => {
                    let node = *self.nodes.get(alias.0 as usize).ok_or(CheckError::Malformed(Malformed::TypeOrder { node: alias.0, child: alias.0 }))?;
                    let K::Alias(rigid, SAlias::Projection, def, l) = node else { continue };
                    if let Some(l) = self.self_swapped(l, opaque, assoc)? {
                        let alias = STy(self.nodes.len() as u32);
                        self.nodes.push(K::Alias(rigid, SAlias::Projection, def, l));
                        self.preds[i] = Pred::Projection { alias, term };
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// `l` with its first term, the opaque `opaque` at arguments `X`, made the projection
    /// `assoc` at `X` (a new node and list; `l` is left as it is, other answers may hold it).
    /// `None` when `l` is empty or its first term is not that opaque.
    fn self_swapped(&mut self, l: SList, opaque: ItemKey, assoc: SDef) -> Result<Option<SList>, Stop> {
        if l.len == 0 {
            return Ok(None);
        }
        let malformed = || -> Stop { CheckError::Malformed(Malformed::TypeOrder { node: l.start, child: l.start }).into() };
        let first = *self.lists.get(l.start as usize).ok_or_else(malformed)?;
        let node = *self.nodes.get(first.0 as usize).ok_or_else(malformed)?;
        let K::Alias(rigid, SAlias::Opaque, def, args) = node else { return Ok(None) };
        if def != SDef(opaque.0) {
            return Ok(None);
        }
        let projection = STy(self.nodes.len() as u32);
        self.nodes.push(K::Alias(rigid, SAlias::Projection, assoc, args));
        let start = self.lists.len() as u32;
        self.lists.push(projection);
        for j in 1..l.len {
            let t = *self.lists.get((l.start + j) as usize).ok_or_else(malformed)?;
            self.lists.push(t);
        }
        Ok(Some(SList { start, len: l.len }))
    }

    /// Elaborates the item bounds at `self.preds[start..]` in place: for each trait bound
    /// `X: Tr<args>` there (those appended here too, so transitively), `Tr`'s supertrait
    /// clauses with `Tr`'s parameters replaced by `X, args`, each trait bound once. Returns a
    /// digest of the trait definitions read, for the row's digest.
    pub(super) fn elaborate_item_bounds<W: World>(&mut self, start: usize, reads: &mut Reads<'_, '_, W>) -> Result<u64, Stop> {
        let mut read = 0u64;
        let mut put: Vec<u32, blaze_own::Alloc<'a>> = Vec::new_in(self.alloc);
        let mut with: Vec<STy, blaze_own::Alloc<'a>> = Vec::new_in(self.alloc);
        let mut i = start;
        while i < self.preds.len() {
            let p = self.preds[i];
            i += 1;
            let Pred::Trait { trait_, args, polarity: Polarity::Positive } = p else { continue };
            let k = ItemKey(trait_.0);
            let q = Question::TraitDef(k);
            let Some(t) = reads.trait_def(k)? else { return Err(CheckError::Malformed(Malformed::NoRecord(q)).into()) };
            read = read.rotate_left(1) ^ digest("blaze-typeck item bound supertraits", &reads.digest(q)?);
            if t.supertraits.is_empty() {
                continue;
            }
            with.clear();
            for j in 0..args.len {
                let a = *self.lists.get((args.start + j) as usize).ok_or(CheckError::Malformed(Malformed::NoRecord(q)))?;
                with.push(a);
            }
            let c = Conv { q, tab: INSTANCE_TAB, t: t.types };
            put.clear();
            for cl in t.supertraits {
                let Some(sp) = self.instantiate(c, k, *cl, &with, &mut put)? else { continue };
                if self.bound_listed(start, sp) {
                    continue;
                }
                if self.preds.len() - start >= MAX_ELABORATED {
                    return Err(CheckError::Limit(Limit::Supertraits).into());
                }
                self.preds.push(sp);
            }
            for n in put.iter() {
                self.memo.remove(&(INSTANCE_TAB, *n));
            }
        }
        Ok(read)
    }

    /// Whether the trait bound `p` is in `self.preds[from..]` already: the same trait and
    /// polarity over the same terms. A projection or const clause is never the same (its
    /// terms are made anew): it is listed again, which changes no answer.
    fn bound_listed(&self, from: usize, p: Pred) -> bool {
        let Pred::Trait { trait_, args, polarity } = p else { return false };
        let listed = self.preds.get(from..).unwrap_or(&[]);
        listed.iter().any(|q| match *q {
            Pred::Trait { trait_: t2, args: a2, polarity: p2 } => {
                t2 == trait_
                    && p2 == polarity
                    && a2.len == args.len
                    && (0..args.len).all(|j| self.lists.get((args.start + j) as usize) == self.lists.get((a2.start + j) as usize))
            }
            _ => false,
        })
    }

    /// The clause `cl` of `c`'s table (trait `of`'s supertraits) as a solver predicate, `Param(i)`
    /// and `ConstParam(i)` replaced by `with[i]` (`FactCache::clauses`, substituting). `None`: a
    /// `where const` clause under the planted bug that drops them.
    fn instantiate(
        &mut self,
        c: Conv<'_>,
        of: ItemKey,
        cl: PClause,
        with: &[STy],
        put: &mut Vec<u32, blaze_own::Alloc<'a>>,
    ) -> Result<Option<Pred>, Stop> {
        let p = match cl {
            PClause::Trait { trait_, args } => {
                let site = crate::error::ParamSite::Supertrait { of, super_: trait_ };
                Pred::Trait { trait_: SDef(trait_.0), args: self.instance_list(c, args, with, put, site)?, polarity: Polarity::Positive }
            }
            PClause::Projection { trait_, args, name, term } => {
                let site = crate::error::ParamSite::Supertrait { of, super_: trait_ };
                let d = assoc_def(trait_, name);
                self.names.insert(d, (trait_, name));
                let args = self.instance_list(c, args, with, put, site)?;
                let term = self.instance_ty(c, term, with, put, site)?;
                let alias = self.nodes.len() as u32;
                self.nodes.push(K::Alias(Rigid::No, SAlias::Projection, SDef(d), args));
                Pred::Projection { alias: STy(alias), term }
            }
            PClause::Const { cond, args } => {
                #[cfg(test)]
                if self.drop_const_clauses {
                    return Ok(None);
                }
                let site = crate::error::ParamSite::Supertrait { of, super_: cond };
                Pred::ConstHolds { cond: SDef(cond.0), args: self.instance_list(c, args, with, put, site)? }
            }
            PClause::ForFields { container, trait_, args } => {
                let site = crate::error::ParamSite::Supertrait { of, super_: trait_ };
                Pred::ForFields {
                    container: self.instance_ty(c, container, with, put, site)?,
                    trait_: SDef(trait_.0),
                    args_after_self: self.instance_list(c, args, with, put, site)?,
                }
            }
            // Outlives clauses are the borrow checker's (D148); the checker takes them as true.
            PClause::TypeOutlives { .. } | PClause::RegionOutlives { .. } => return Ok(None),
        };
        Ok(Some(p))
    }

    /// The list `l` of `c`'s table, each term instantiated ([`Self::instance_ty`]); `site` is
    /// named by a parameter past `with`.
    fn instance_list(
        &mut self,
        c: Conv<'_>,
        l: crate::ids::List,
        with: &[STy],
        put: &mut Vec<u32, blaze_own::Alloc<'a>>,
        site: crate::error::ParamSite,
    ) -> Result<SList, Stop> {
        let mut terms: Vec<STy, blaze_own::Alloc<'a>> = Vec::with_capacity_in(l.len as usize, self.alloc);
        for i in l.range() {
            let p = c.t.list_slot(i as usize).ok_or(CheckError::Malformed(Malformed::NoRecord(c.q)))?;
            terms.push(self.instance_ty(c, p, with, put, site)?);
        }
        let start = self.lists.len() as u32;
        self.lists.extend_from_slice(&terms);
        Ok(SList { start, len: l.len })
    }

    /// The solver term of `root` of `c`'s table with `Param(i)` and `ConstParam(i)` replaced by
    /// `with[i]`: `FactCache::ty`'s walk (children first, by a work list; a child comes before
    /// its parent in the table, so it ends) under the instance's memo number, each node it
    /// enters there named in `put`. A parameter past `with` is refused by name.
    fn instance_ty(
        &mut self,
        c: Conv<'_>,
        root: PTy,
        with: &[STy],
        put: &mut Vec<u32, blaze_own::Alloc<'a>>,
        site: crate::error::ParamSite,
    ) -> Result<STy, Stop> {
        self.work.clear();
        self.work.push((root.0, false));
        while let Some((n, expanded)) = self.work.pop() {
            if self.memo.contains_key(&(c.tab, n)) {
                continue;
            }
            let node = c.t.node(PTy(n)).map_err(Stop::from)?;
            if let PNode::Param(i) | PNode::ConstParam(i) = node {
                let Some(&s) = with.get(i as usize) else {
                    return Err(CheckError::Malformed(Malformed::Param { index: i, args: with.len() as u32, site }).into());
                };
                self.memo.insert((c.tab, n), s);
                put.push(n);
                continue;
            }
            if !expanded {
                self.work.push((n, true));
                self.kids.clear();
                crate::portable::node_children(c.t, node, &mut self.kids)?;
                for &ch in &self.kids {
                    if ch >= n {
                        return Err(CheckError::Malformed(Malformed::TypeOrder { node: n, child: ch }).into());
                    }
                    self.work.push((ch, false));
                }
                continue;
            }
            let k = self.convert(c, node)?;
            let s = STy(self.nodes.len() as u32);
            self.nodes.push(k);
            self.memo.insert((c.tab, n), s);
            put.push(n);
        }
        self.memo.get(&(c.tab, root.0)).copied().ok_or(CheckError::Malformed(Malformed::TypeOrder { node: root.0, child: root.0 }).into())
    }
}
