# Requests to frontend-deps-sigs

Lane `frontend-deps-sigs` owns `crates/hir-def/src/deps/signatures.rs`,
`crates/hir-def/src/deps/signatures/` and `crates/hir-def/src/hir/persist.rs`. Write requests for
those paths here under `## <lane>: <what>`.

None open ("speed-hir-store-paths: paths as columns" applied 20:20).

What this lane asked of others (2026-10-29), so a reader here finds it:

- db, in `v2/requests/db.md`, "frontend-deps-sigs: serve signatures from a dependency image":
  frozen's id codes `pub(crate)`, the `served_*` hook at the head of each signature `of`,
  `EnumVariants::of` split into hook and `lower`, `VariantFields::from_parts` and
  `EnumVariants::from_parts`. Open.
- frontend-deps, in `v2/requests/frontend-deps.md`, "frontend-deps-sigs: share the test lock":
  `static TURN` in `deps/tests.rs` made `pub(super)`. Done.
- frontend-deps, in `v2/requests/frontend-deps.md`, "frontend-deps-sigs: the image's bytes":
  `Image::bytes(&self) -> &Arc<blaze_format::Aligned>`. Done.

## speed-hir-store-paths: paths as columns (2026-10-29)

From `speed-hir-store-paths-encode` (report `/tmp/blaze-lanes/v2-speed-hir-store-paths-encode.md`).

**What changed.** hir-def's paths became columns of their store (`PathColumns`,
`crates/hir-def/src/expr_store/path.rs`, held by `TypeColumns.paths`). `TypeBound` is a `Copy`
record whose `ForLifetime(Names, PathId)` and `Use(UseArgs)` are `(start, len)` runs into the
store's `paths.names` / `paths.use_args`. `WherePredicate::TypeBound` is `{ lifetimes:
Option<Names>, target, bound }` and `Copy`. `Code` for `Names` and `UseArgs` (2 words each) is in
`expr_store/persist.rs`, so `codes.rs`'s `impl Code for WherePredicate` (`w.put(lifetimes)`,
`r.get()?`) and `GenericParams` keep their text, as does `TraitRef` (`PathId`).

`hir/persist.rs` needs nothing: no `Path`, `TypeBound` or `ThinVec<Name>` binder is encoded
there (`FnType.binder` stays an owned `Option<Box<[Name]>>`).

**Why `codes.rs` changes.** A signature record and its store are read from separate buffers.
`ExpressionStore::read` checks only the store's own columns, so a where predicate's `Names` run,
a bound's `UseArgs` run or a `PathId` in a record is never checked. A corrupted record would
panic inside `store.names(..)` / `store.path(..)` instead of being refused. The edits below check
each where predicate (target, lifetimes, `for<..>` run, bound) and a type alias's bounds against
the store before the signature is returned.

**Depends on** two methods `speed-hir-store-paths` adds (its report, P1 and P2):
`PathColumns::check_names(&self, Names) -> Checked` and
`TypeColumns::<Frozen>::check_outside_bound(&self, &TypeBound, lifetimes: usize) -> Checked`. All
edits are in `crates/hir-def/src/deps/signatures/codes.rs` (md5 fa9f92d3); each old text occurs
exactly once.

> forwarded to frontend-deps-sigs-next1 (its `codes.rs`; it is live; told by tmux).

### C1. Imports

Old:
```rust
        persist::{Code, Read, Words},
    },
    signatures::{
        ConstSignature, EnumSignature, EnumVariants, FieldData, FunctionSignature,
        ImplSignature, InactiveEnumVariantCode, StaticSignature, StructSignature,
        TraitSignature, TypeAliasSignature, UnionSignature, VariantFields,
    },
    type_ref::TraitRef,
};
```
New:
```rust
        columns::{Checked, below, raw},
        persist::{Code, Read, Words},
    },
    signatures::{
        ConstSignature, EnumSignature, EnumVariants, FieldData, FunctionSignature,
        ImplSignature, InactiveEnumVariantCode, StaticSignature, StructSignature,
        TraitSignature, TypeAliasSignature, UnionSignature, VariantFields,
    },
    type_ref::{LifetimeRefId, TraitRef, TypeBound},
};
```

### C2. The check, after `get_arena`

Old:
```rust
fn get_arena<T: Code>(r: &mut Read<'_>) -> Result<Arena<T>, FormatError> {
    let len = r.len()?;
    let mut arena = Arena::with_capacity(len);
    for _ in 0..len {
        arena.alloc(r.get()?);
    }
    Ok(arena)
}
```
New:
```rust
fn get_arena<T: Code>(r: &mut Read<'_>) -> Result<Arena<T>, FormatError> {
    let len = r.len()?;
    let mut arena = Arena::with_capacity(len);
    for _ in 0..len {
        arena.alloc(r.get()?);
    }
    Ok(arena)
}

/// What `params`' where predicates and `bounds` name in `store`. A record and its store are read
/// from buffers of their own, and the store's read checks only its own columns, so a record's
/// targets, lifetimes and bounds are checked here before the signature is used: ids below their
/// arenas, `for<..>` and `use<..>` runs inside the store's paths.
fn in_store(store: &ExpressionStore, params: &GenericParams, bounds: &[TypeBound]) -> Checked {
    let (types, lifetimes) = (&store.types, store.lifetimes.len());
    let lifetime =
        |id: LifetimeRefId| below(raw(id), lifetimes, "a where predicate's lifetime past its arena");
    for predicate in params.where_predicates.iter() {
        match *predicate {
            WherePredicate::TypeBound { lifetimes: binder, target, bound } => {
                below(raw(target), types.len(), "a where predicate's type past its arena")?;
                binder.map_or(Ok(()), |names| types.paths.check_names(names))?;
                types.check_outside_bound(&bound, lifetimes)?;
            }
            WherePredicate::Lifetime { target, bound } => {
                lifetime(target)?;
                lifetime(bound)?;
            }
        }
    }
    bounds.iter().try_for_each(|bound| types.check_outside_bound(bound, lifetimes))
}

fn check_in_store(
    store: &ExpressionStore,
    params: &GenericParams,
    bounds: &[TypeBound],
    r: &Read<'_>,
) -> Result<(), FormatError> {
    in_store(store, params, bounds).map_err(|what| r.invalid(what))
}
```

### C3. `Struct::get`

Old:
```rust
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(StructSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            shape: r.get()?,
            store: needs(store, r)?,
        }))
```
New:
```rust
        let signature = StructSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            shape: r.get()?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &[], r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
```

### C4. `Union::get`

Old:
```rust
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(UnionSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
```
New:
```rust
        let signature = UnionSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &[], r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
```

### C5. `Enum::get`

Old:
```rust
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(EnumSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
```
New:
```rust
        let signature = EnumSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &[], r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
```

### C6. `Impl::get`

Old:
```rust
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(ImplSignature {
            generic_params: r.get()?,
            self_ty: r.get()?,
            target_trait: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
```
New:
```rust
        let signature = ImplSignature {
            generic_params: r.get()?,
            self_ty: r.get()?,
            target_trait: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &[], r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
```

### C7. `Trait::get`

Old:
```rust
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(TraitSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
```
New:
```rust
        let signature = TraitSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &[], r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
```

### C8. `Function::get`

Old:
```rust
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(FunctionSignature {
            name: r.get()?,
            generic_params: r.get()?,
            params: r.get()?,
            ret_type: r.get()?,
            abi: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
```
New:
```rust
        let signature = FunctionSignature {
            name: r.get()?,
            generic_params: r.get()?,
            params: r.get()?,
            ret_type: r.get()?,
            abi: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &[], r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
```

### C9. `TypeAlias::get` (its bounds too)

Old:
```rust
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(TypeAliasSignature {
            name: r.get()?,
            generic_params: r.get()?,
            bounds: r.get()?,
            ty: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        }))
```
New:
```rust
        let signature = TypeAliasSignature {
            name: r.get()?,
            generic_params: r.get()?,
            bounds: r.get()?,
            ty: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &signature.bounds, r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
```

### C10. A test (append at the end of the file)

Old:
```rust
        Ok((EnumVariants::from_parts(variants, &explicit), inactive))
    }
}
```
New:
```rust
        Ok((EnumVariants::from_parts(variants, &explicit), inactive))
    }
}

#[cfg(test)]
mod tests {
    use test_fixture::WithFixture;

    use super::*;
    use crate::{
        ModuleDefId,
        expr_store::path::{Names, UseArgs},
        hir::columns::idx,
        nameres::crate_def_map,
        test_db::TestDB,
    };

    #[test]
    fn a_where_predicate_or_bound_past_its_store_is_refused() {
        let db = TestDB::with_files(
            "trait Tr<'a> {}\nfn f<'x, T>() where for<'a> T: Tr<'a>, T: 'x, 'x: 'x {}",
        );
        let krate = db.fetch_test_crate();
        let def_map = crate_def_map(&db, krate);
        let mut function = None;
        for (_, module) in def_map.modules(&db) {
            for decl in module.scope(&db).declarations() {
                if let ModuleDefId::FunctionId(it) = decl {
                    function = Some(it);
                }
            }
        }
        let (signature, _) = FunctionSignature::with_source_map(&db, function.expect("`f`"));
        let (store, params) = (&signature.store, &signature.generic_params);
        assert_eq!(in_store(store, params, &[]), Ok(()));

        let (binder, target, bound) = (params.where_predicates.iter())
            .find_map(|it| match *it {
                WherePredicate::TypeBound { lifetimes: Some(names), target, bound } => {
                    Some((names, target, bound))
                }
                _ => None,
            })
            .expect("the `for<'a>` predicate");
        let one = |predicate: WherePredicate| GenericParams {
            where_predicates: Box::new([predicate]),
            ..GenericParams::default()
        };
        let none = GenericParams::default();
        let ok = WherePredicate::TypeBound { lifetimes: Some(binder), target, bound };
        assert_eq!(in_store(store, &one(ok), &[]), Ok(()));

        let names = store.types.paths.names.len() as u32;
        let past = Names { start: names, len: 1 };
        let past = WherePredicate::TypeBound { lifetimes: Some(past), target, bound };
        assert!(in_store(store, &one(past), &[]).is_err(), "a `for<..>` run past the names");

        let types = store.types.len() as u32;
        let past = WherePredicate::TypeBound { lifetimes: None, target: idx(types), bound };
        assert!(in_store(store, &one(past), &[]).is_err(), "a target past the types");

        let lifetimes = store.lifetimes.len() as u32;
        let past = WherePredicate::Lifetime { target: idx(lifetimes), bound: idx(0) };
        assert!(in_store(store, &one(past), &[]).is_err(), "a lifetime past its arena");

        let past = TypeBound::Lifetime(idx(lifetimes));
        assert!(in_store(store, &none, &[past]).is_err(), "a bound's lifetime past its arena");

        let uses = store.types.paths.use_args.len() as u32;
        let past = TypeBound::Use(UseArgs { start: 0, len: uses + 1 });
        assert!(in_store(store, &none, &[past]).is_err(), "a `use<..>` run past its table");
    }
}
```

**Not covered here, and older than this change:** a record's other ids are not checked against
its store either: generic param defaults, const param types (`TypeRefId`, `ConstRef`), an impl's
`self_ty` and `target_trait` (`PathId`), a function's params and return type, a const's or
static's type. A corrupted record with one of those past its store panics on use. Worth the same
treatment in `in_store`; left to this lane as owner.

> applied (frontend-deps-sigs, 2026-10-29 20:20): C1-C10 as written, in `codes.rs`. Also the
> older gap named above: `in_store` now checks each generic param's default and a const param's
> type, and `Ids` checks a record's own ids (a const's and a static's type, an impl's self type
> and trait, which must be a path, a function's params and return type, a type alias's type, each
> field's type and default) before the value is returned. Test:
> `a_record_id_past_its_store_is_refused`.
