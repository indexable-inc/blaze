# Requests from requests-goal-reads (applying requests-goals.md's `Implements` and `Specializes` reads)

## requests-goal-reads, for db-ty (`crates/hir-ty/src/next_solver/interner.rs`, `crates/hir-ty/src/db.rs`): the solver reads `relevant_impls` (2026-10-29)

**What.** `crates/hir-ty/src/method_resolution.rs` now has `relevant_impls(db, RelevantImplsKey)`
(tracked, `returns(ref)`), keyed by one interned
`RelevantImplsKey { krate: Crate, trait_: TraitId, self_ty: Option<SelfTyKey<'db>> }`. It returns
`RelevantImpls`, with `all()`, `for_self_ty()` and `blanket()`: exactly the impls
`DbInterner::for_each_relevant_impl` finds today for one simplified self type, in its order (crate
and dependency impls, then the block impls not seen, then `for_each_blanket_impl`'s blanket
impls), and with `self_ty: None` the blanket impls alone (`for_self_ty()` empty). A test,
`method_resolution/relevant_tests.rs`, compares it with today's walk, copied verbatim.

1. In `interner.rs`, `for_each_relevant_impl`: drop the closure `consider_impls_for_simplified_type`
   and read the query instead.

   ```rust
   let trait_def_id = trait_ref.def_id;
   let self_ty = trait_ref.self_ty();
   let krate = self.krate.expect("trait solving requires setting `DbInterner::krate`");
   let relevant = |simp: SimplifiedType<'db>| {
       let key = RelevantImplsKey::new(self.db, krate, trait_def_id.0, Some(SelfTyKey(simp)));
       relevant_impls(self.db, key)
   };
   match self_ty.kind() {
       TyKind::Bool
       | /* ...the same rigid arms as today... */
       | TyKind::UnsafeBinder(_) => {
           let simp =
               fast_reject::simplify_type(self, self_ty, fast_reject::TreatParams::AsRigid)
                   .unwrap();
           // The blanket impls are among them: no second read.
           for &impl_ in relevant(simp).all() {
               try_visit!(f(impl_));
           }
           return R::output();
       }
       TyKind::Infer(InferTy::IntVar(_)) => {
           // ...`possible_integers` as today...
           for simp in possible_integers {
               for &impl_ in relevant(simp).for_self_ty() {
                   try_visit!(f(impl_));
               }
           }
       }
       TyKind::Infer(InferTy::FloatVar(_)) => {
           // ...`possible_floats` as today...
           for simp in possible_floats {
               for &impl_ in relevant(simp).for_self_ty() {
                   try_visit!(f(impl_));
               }
           }
       }
       // the alias/placeholder/error, coroutine-witness and panicking arms as today
   }
   self.for_each_blanket_impl(trait_def_id, f)
   ```

2. `for_each_blanket_impl`:

   ```rust
   let Some(krate) = self.krate else {
       return R::output();
   };
   let key = RelevantImplsKey::new(self.db, krate, trait_def_id.0, None);
   for &impl_ in relevant_impls(self.db, key).blanket() {
       try_visit!(f(impl_));
   }
   R::output()
   ```

   The import `method_resolution::{SelfTyKey, TraitImpls}` becomes
   `method_resolution::{RelevantImplsKey, SelfTyKey, relevant_impls}`. `TraitImpls` and the
   `AdtId` arms used only by the old closure may become unused there.

3. In `db.rs` `kinds`, four new names, or `every_query_has_a_kind` fails:
   - in "Method resolution, impls, coherence, specialization":
     `RELEVANT_IMPLS = 0x247b, "relevant_impls";` and `MAY_SPECIALIZE = 0x247c, "may_specialize";`
     (`specialization.rs`: the feature check `specializes` and `specialized_ancestor` read, now
     keyed by the impl);
   - in "Interned ids": `RELEVANT_IMPLS_KEY = 0x25f5, "RelevantImplsKey";` and
     `IMPL_PAIR = 0x25f6, "ImplPair";` (`specializes_query`'s key, `specialization.rs`).

**Why.** The solver reads `TraitImpls::in_crate_and_deps_for_trait_and_self_ty` (three
arguments), `TraitImpls::for_block` (a whole block's table) and
`TraitImpls::blanket_in_crate_and_deps` (two arguments). None of these reads names one trait and
one self type that a fact could stand for, so the persistent goal cache keeps every goal that
selects among impls out of the store (docs/v2/plan/lanes/frontend/fast-trait-store.md 3.4, row 1).
A read of `relevant_impls` decodes to `(crate, trait, self type)`, and blaze-items'
`fact_of_read` maps it to `Fact::Implements { trait_, self_ty }`; with no self type, it maps to
`Implements` for the self type that only blanket impls unify with. The answers stay the same: the
query is the walk the solver did, moved into one memo.

> forwarded to db-ty (db-ty-next2, told by requests-goal-reads) (requests, pass 31).
