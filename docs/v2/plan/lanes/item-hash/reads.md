---
status: plan
summary: lane item-hash-facts — every query and input a body's inference, MIR building and borrow check read directly, which step reads it, what `blaze_item_hash::reads::classify` makes of it (covered by the identity, a fact, or refused) and why that is exact; the facts this lane adds; the refusals and what lifts each; the cross-check against lane firewall's exceptions.
date: 2026-10-30
---
# What each read of a body's steps stands for

Design: [item-hash.md](../../design/item-hash.md) sections 4 to 6. Code:
`crates/blaze-item-hash/src/reads.rs` (`classify`, the `RULES` table), `values.rs` (the values
of the facts), `tests/classify.rs`.

A step's record is reused when its identity and the values of the facts its shape lists are
what they were. So a read may be left out of the shape only if what the step does with it is a
function of the identity (**covered**), and a fact stands for a read only if what the step does
with the read is a function of the fact's value (**fact**). Anything else is **refused**: the
step is not stored. A refusal never gives a wrong answer; a wrong rule here does, so a rule is
added only with the argument below and a fixture pair in `tests/classify.rs`.

## How a read is found

`salsa::capture_reads` gives each read as an ingredient index, a key and the ingredient's debug
name. `classify` looks the name up in `RULES`, sorted, by binary search, and then checks the end
of the path of the file the ingredient was declared in (`Ingredient::location().file`): a field
of a salsa struct is named by the field alone (`data`, `scope`, `table`), and two crates may each
have a function of one name. A rule then reads its key as one type by the key's page
(`SalsaStructInDb::cast`); a key of another type is refused. Interned reads are covered (an
interned value is a function of its fields, which whoever interned it read); accumulator reads
are refused.

## The facts this lane adds

Kinds `0x1800..0x1810` of `blaze_query::kinds::FACTS`, reserved by request 00814.

| fact | kind | value | why lane items' facts do not do |
|---|---|---|---|
| `Fields(item)` | `0x1800` | the item's signature hash (no body is inferred for it, so the owner's own inference may ask it), then the names of its members in order: a struct's or union's fields (and whether each has a default), an enum's variants with their fields, a trait's or impl's items with their kinds | the signature hash names members by place (`blaze_items::form`): renaming a field, a variant, or a trait method with its impls keeps it |
| `Attrs(owner)` | `0x1801` | all of the owner's `AttrFlags` bits, its `#[unstable]` feature, a function's target features sorted | the interface hash leaves out the flags `form::unobserved` lists, and inference reads one of them: `DIAGNOSTIC_DO_NOT_RECOMMEND`, in the trait error it reports (`next_solver/infer/errors.rs`) |
| `CrateRoot(crate)` | `0x1802` | the crate's lang items by item key, the crates it loads and depends on by crate key (itself as a mark), its `#![feature]`s sorted, its recursion limit | `Crate(crate)` hashes every file of the crate, so for the body's own crate it changes with every edit |

A subject that no longer resolves has the value `values::absent(kind)`; a fact of one of these
kinds with an object, or of a kind of the range no fact has, has no value.

## The table

Steps: **I** inference (`infer_body_query` and what it calls inline: `infer.rs`, `infer/`,
`method_resolution`, the solver, type lowering, the resolver), **M** MIR building
(`mir_body_query` in `providers.rs`, `hir-ty-mir/src/mir/lower*`), **B** the borrow check
(`hir-ty-borrowck`).

### Covered

| query or input | steps | when | why exact |
|---|---|---|---|
| `Body::of`, `ExprScopes::body_expr_scopes`, `body_upvars_mentioned`, `Body::has_lowering_errors` | I M B | keyed by the body itself; another body's is refused | the identity has the body's codec bytes; scopes and upvars are functions of them and of the resolutions, which it has. Whether lowering found an error is not of the HIR (an undeclared label or a literal too large can lower as if there were none), so the identity's `hir` part hashes that bool beside the HIR |
| `crate_local_def_map`, `block_def_map`, `DefMapPair`, `def_map`; `ModuleScope`, `scope`, `children`, `ModuleScope::{name, legacy_macro, traits}`; `Modules`, `Modules::{module, name}`; `CrateResolution::module` (`nameres/delta/publish.rs`); `ModuleIdLt`, `BlockIdLt`; `row` (`nameres/facts/queries.rs`); `crate_shadows_primitive_type`; `module_trait_set` | I M | any key | name resolution: the identity hashes what each path of the body resolves to, the traits in scope at each scope and the module chain (section 3); these reads are how a step finds what the identity already says. Agreed with lane item-hash-identity's `covered_queries()` |
| `Crate` input fields `data`, `extra_data`, `workspace_data`, `cfg_options` | I M B | the body's crate; another crate's is its `Crate` fact | the identity hashes the crate's key (origin, dependencies, cfg), edition and workspace data (`identity::crate_inputs`). `env`, `content` and `attr_files` are not hashed there and stay refused |
| `lang_items`, `UnstableFeatures::query`, `crate_recursion_limit`, `loaded_crates`; `crate_lang_items`, `goal_crate_key`, `crate_key`; `target_data_layout_query` | I M B | the body's crate; another crate's is its `CrateRoot`, `Crate` or `Target` fact | `identity::crate_inputs` hashes the goal store's crate key (lang items by key, loaded crates), the features, the recursion limit and the workspace data |
| a `Signature` fact of the body's owner, or of the impl or trait holding it | I M B | after any rule below gives it | the identity has both signature hashes (section 3.2); no read of a hidden type may come here |
| an interned value | all | always | a function of its fields |

`Modules.table` is not covered: a direct read of it is a walk over every module of a crate,
which no path's resolution stands for (`hir-ty/src/lower/path.rs`, `inherent_associated_type`,
walks every module's inherent impls inline in inference). It is refused.

### Facts, lane items' (through `blaze_items::goals::Goals::fact_of`)

| query | steps | fact | why exact |
|---|---|---|---|
| `ImplSignature::kept`, `impl_trait_of_impl`, `impl_self_ty_of_impl`, `impl_has_unconstrained_params` | I B | `Signature(impl)` | the impl's header and generics are its interface |
| `StructSignature::kept`, `EnumSignature::kept`, `UnionSignature::kept`, `fn_sig_for_struct_constructor`, `fn_sig_for_enum_variant_constructor`, `field_types_of_variant`, `ctor_variances_query`, `representability` | I M B | `Signature(adt)` | flags, generics, field types and shapes are the ADT's interface; a name read is the key's |
| `TraitSignature::kept`, `supertraits_info`, `dyn_compatibility_of_trait_query` | I | `Signature(trait)` | the trait's header, supertraits and items' signatures are its interface |
| `FunctionSignature::kept`, `fn_sig_for_fn` | I M B | `Signature(fn)` | the signature is its interface |
| `rpit_hidden_types`, `tait_hidden_types` | I M B | `HiddenTypes(fn or alias)` (lane items' `fact_of_read`) | a hidden type, which the signature hash marks (section 4) |
| `const_eval_static_query`, `const_eval_discriminant_variant` | I M | `ConstValue` of the static or the variant | a value, which the signature hash marks (section 4) |
| `fact_value` (lane items) | I M B | the fact its key names | the value of that fact |
| `generic_predicates_of`, `variances_of_query`, `const_conditions_query` | I B | `Signature(generic def)` | predicates by interface: a supertrait's change is the trait's interface's |
| `ItemBounds::of` | I | `Signature` of the bounds' owner | as lane items defines |
| `target_data_layout_query` | I M | `Target(crate)` | the target is the crate's |
| `crate_impl_traits`, `crate_impl_self_tys`, `crate_field_types`, `crate_impl_predicates` | I | `Crate(dependency)` | a dependency's batch, lowered for the whole crate |
| `relevant_impls` | I | `Implements` | the relevant impl set |
| `may_specialize`; `specializes_query` | I | `Specializes(impl)`; `Crate` of the specializing impl's crate | as lane items defines |

### Facts, this lane's

| query | steps | fact | why exact |
|---|---|---|---|
| `ConstSignature::kept`, `type_for_const_with_diagnostics` | I M | `Signature(const)` | the const's type and generics are its interface |
| `StaticSignature::kept`, `type_for_static_with_diagnostics` | I M | `Signature(static)` | as above |
| `TypeAliasSignature::kept`, `type_for_type_alias_with_diagnostics` | I | `Signature(alias)` | as above |
| `type_alias_bounds_with_diagnostics` | I | `Signature` of the trait for an associated type, else of the alias | a trait hashes its associated types' bounds |
| `generic_defaults_with_diagnostics`, `const_param_types_with_diagnostics`, `trait_environment_query`, `generics_require_sized_self` | I M B | `Signature(generic def)` | defaults, const parameter types and predicates are the interface |
| `elided_lifetimes` | I B | `Signature(fn)` | from the signature |
| `VariantFields::field_visibilities` | I | `Signature(adt)` | the interface hashes each field's visibility relative to its module, and the module chain is the identity's |
| `AssocItemId::assoc_visibility` | I | `Signature(item)` | the item's interface hashes its visibility |
| `VariantFields::kept`, `EnumVariants::of`, `TraitItems::query_with_diagnostics`, `ImplItems::of` | I M B | `Fields` of the ADT (an enum variant's enum), trait or impl | names, which the interface hash leaves out; the rest of what a step uses of them is in the interface hash `Fields` hashes |
| `AttrFlags::query`, `unstable_feature` (`attrs.rs`) | I M | `Attrs(owner)` | the value hashes every flag and the feature |
| `target_features` (`attrs.rs`) | I | `Attrs(fn)` | the value hashes the sorted features |
| `transitive_deps`; another crate's `lang_items`, `UnstableFeatures::query`, `crate_recursion_limit`, `loaded_crates` | I M B | `CrateRoot(crate)` | each is one part of the value |
| `TraitImpls::for_crate`, `TraitImpls::for_crate_and_deps`, `InherentImpls::for_crate`, `crates_containing_incoherent_inherent_impls`, `macros_writing_impls`, `diagnostic_items` | I B | `Crate(crate)` | a table of all of a crate and its dependencies, which the fact hashes |
| `TraitImpls::for_module`, `InherentImpls::for_module`, `module_impls` | I | `Crate` of the module's crate | the module's impls are part of its crate |
| `destructor` | I B | `DropCheck(adt)` | the value hashes which impl is the `Drop` impl |
| `orphan_rules_hold` | I | `Coherence(impl)` | the value hashes the orphan check's result |
| `predicates` (`builtin_derive.rs`) | I B | `Derive(adt)` | the value hashes each built-in derive impl's interface |
| the tables above of impls and traits by several arguments (salsa's `interned_arguments`, 00827), of another crate | I | `Crate` of the crate they name (of a module's crate for `module_items_named`) | the `Crate` fact hashes the crate and every crate it depends on |
| `impl_value_item`, `trait_value_item` | I | `Fields(impl or trait)` | as `ImplItems::of` and `TraitItems` |
| `impl_type_aliases`, `trait_type_aliases` | I | `Fields(impl or trait)` | the associated types in order, a function of the members' kinds and names `Fields` hashes |
| `closure_exprs` of the body's own store | I | covered (with the identity's own-body reads) | its closures are of its HIR |
| `line_literals` of the body's own | M | covered (with the identity's own-body reads) | which literals `line!()` made is of the HIR and of each literal's macro call, which the entries hash with its macro |
| `failed_expansion` of the body's own | M | covered (with the identity's own-body reads) | whether each macro call expanded as rustc parses it is of the calls the entries hash; a body refused for one builds no MIR to store |
| `const_eval_query` of a const without generics, with no arguments and no environment | I M | `ConstValue(const)` | the evaluation lane items hashes |
| `infer_body_query` of the body itself, from M or B | M B | `StepInfer(body)` (0x1803), valued by `hir_ty::body_store::step_value` | the earlier step's answer, as the store encodes it |
| `mir_body_query` of the body itself, and `mir_body_for_closure_query` and `mir_body_for_coroutine_query` of a closure, async closure or async block written in it, from B | B | `StepMir(body)` (0x1804) | the step value hashes the body's built MIR and every such closure's and coroutine's, in expression order |
| `match_check` of the body itself | B | `MatchCheck(body)` (0x1805) | the value hashes whether match checking failed, the one part of it the borrow check reads (to skip the body) |

`AttrFlags::query` and the two member lists are lane items' `fact_of_read` too, as
`Signature`; this table takes them first, as `Signature` misses a rename and the unobserved
flags (told to lane items).

### Refused

Each is in `REFUSALS` in `tests/classify.rs`, which fails on a refused read not listed.

| query | steps | why | what lifts it |
|---|---|---|---|
| `infer_body_query`, `mir_body_query`, `mir_body_for_closure_query`, `mir_body_for_coroutine_query` of another body, of an inline const, or from the same or an earlier step | any | a cross-body read (see the firewall check below), or a cycle | nothing: it is a leak |
| `mir_body_for_coroutine_by_move_query`, and the MIR of a coroutine kind other than an async block or closure | B | not in the body's MIR step value | the step value holding it |
| `InherentImpls::in_crate_for_self_ty`, `TraitImpls::in_crate_and_deps_for_trait_and_self_ty`, `TraitImpls::in_crate_for_trait_and_self_ty`, `TraitImpls::blanket_in_crate_and_deps`, `TraitImpls::for_crate_and_trait`, `in_crate_trait_value_items`, `module_items_named` (of the body's own crate) | I | a table of the own crate's impls or traits, which no fact lists (another crate's is its `Crate` fact) | an own-crate fact of an impl list (lane items' `Implements` and `Method` answer selection, not the list) |
| `InherentImpls::in_block_for_self_ty` | I | a block's impls: no fact yet | a block fact |
| `closure_exprs` (of another store) | I | another body's closures | nothing: it is a leak |
| `lookup_impl_method_stored` | I | a method's impl, selected in an environment: no fact names it | a selection fact keyed by the environment |
| `layout_of_ty_query`, `layout_of_adt_query` | I M B | a layout in an environment is no fact the trace has | `Layout(adt)` |
| `const_eval_query` (with arguments or an environment, or of a generic const), `anon_const_eval` | I M | `ConstValue` names a const evaluated as itself only, and no anonymous const | `ConstValue` by arguments |
| `is_copy_query`, `drop_glue_query`, `prove_query`, `normalize_query`, `auto_trait_depth`, `copy_clone_impl` | I B | a type or goal in an environment, or an ADT's auto trait for a crate: no fact names one | goal facts by environment |
| `TraitImpls::for_block`, `InherentImpls::for_block` | I | a block's impls, keyed by a block of another body or the body's own (whose impls are content, but the read cannot tell which) | a rule for the own body's blocks |
| `Modules.table` | I | a walk of every module of a crate (above) | `inherent_associated_type` reading `InherentImpls::for_crate` instead |
| `CrateResolution.modules` (`modules`, hir-def `nameres/delta/publish.rs`) | I | a published crate's whole module table, read by `DefMap::table` for a walk (one module is read through `CrateResolution::module`, covered) | the walk reading a table no path's resolution stands for, as `Modules.table` above |
| `Body::with_source_map`, `Body::relative`, the signatures' `with_source_map` and `relative` | B | positions: the borrow check reads which expressions are synthesized and the lowering diagnostics, through the source map, and the identity has no spans | the identity hashing the synthesized-expression set and the diagnostics' kinds |
| `coroutine_witnesses_query`, `opaque_variances_query` | I | a coroutine's witnesses and a TAIT's hidden type come from other bodies' borrow checks (firewall E3, E7) | facts of them |
| the other `AttrFlags::*` queries | I | read rarely; no rule yet | an `Attrs` value holding them |
| blaze-items' own queries, when the goal store runs inside inference | I | the goal store hashes facts inside the active query; its reads name no one fact | request 00828 (one tracked query per fact value), then `Fact(that fact)` |
| any read no rule names | any | unknown | a rule here, with its argument and a fixture pair |

The impl tables lane dbsp maintains (`hir_ty::method_resolution::delta`: the slices `TraitImplsSlice`, `InherentSlice`, `CandidateSlice`, their `SliceDirectory` and `ImplSlices`, and `trait_slice`, `inherent_slice`, `candidate_slice`, `trait_bodies`, `trait_body_modules(_of)`, `inherent_bodies`, `inherent_body_modules(_of)`, `inherent_body_module_list`, `body_impl_set_hash`, `impl_row_hash`, `blanket_hash`) are read only inside `TraitImpls::for_crate_and_trait`, `InherentImpls::in_crate_for_self_ty`, `in_crate_trait_value_items` and the whole-crate tables, never by a step, so none has a rule: a step sees the outer query. A step that read one directly would be refused, and the closure test would name it.

## Firewall's exceptions against section 4 and this audit

Lane firewall's notes list E1 to E10 as the only reads across bodies. Against design section 4
and the table above:

- E1 (RPIT hidden types), E2 (auto traits of an opaque), E4 (const eval), E6 (closures), E5
  (inlining, codegen only) and specialization are in section 4, and each enters a step here as
  `HiddenTypes` (`rpit_hidden_types`, `tait_hidden_types`), `AutoTraits` (the goal store), a refusal (const eval,
  until a `ConstValue` rule), content (closures), nothing (inlining), `Specializes`.
- E7 (TAIT hidden types) is in section 4 with the RPIT, and a read of it is `HiddenTypes`. E3
  (a coroutine's witnesses and layout) is not; its read (`coroutine_witnesses_query`) is
  refused here, so no step that reads it is stored.
- E8 to E10 are past the front end's steps.
- No rule here covers or maps a read of another body's `infer_body_query`, `mir_body_query`, its
  closures' MIR or `Body::of`: each is refused (`is_own_step` checks the owner), and a test fixture that shows one is a firewall leak to report.
- `inherent_associated_type`'s walk reads other items, not bodies: not the firewall's, but no
  fact covers it, so it is refused.
