---
status: plan
summary: Unison-style identities for blaze. A compiled body's store key holds what it does, never what it or its callees are called or where they are written. A name or place is keyed only by code that reads it. Positions are held by table and filled per instance, and the link keeps per-body identity.
date: 2026-11-04
---
# Name-free identity

Lane namefree. Unison hashes a definition by its content: names are metadata, and a reference is
the hash of what it refers to. blaze's item hashes (`blaze-items`) already work this way. What
follows them does not:
- codegen's code hash;
- the store keys of compiled templates;
- the live-patch slot key;
- compile identity.

All of these hold an `ItemKey`, the item's path and name, or a store key derived from it. Renaming
a function, moving it to another module, or renaming a local therefore recompiles code whose
machine code did not change. The audit is in `~/.cache/blaze/coord/namefree/audit.md`. This doc is
the scheme.

## The rule

A key holds what the value is made of (rule 20). A name or a place is part of a value only where
the code reads it. So:

1. **References by content.** A callee, a type or a constant is keyed by the content hash of its
   resolved definition: its interface hash for a call, and its layout and interface for a type.
   Neither its path nor its name enters the key. Locals and binders are keyed by position
   (binding index, the de Bruijn style), not by spelling.
2. **Names and places where read, and only there.** Each of these reads a name or a place, and
   is keyed by it:
   - `type_name` and `type_id`: `blaze_items::observes_names` adds the item keys of the nominal
     arguments.
   - `module_path!` and `stringify!`: they are literals in MIR.
   - `file!()`: the file's name as the build gives it.
   - `line!()` and `column!()` where they are values: in `#[track_caller]` bodies, consts and
     promoteds, the literal is in MIR.
   - Debug info's told types.
   - An exported or `#[no_mangle]`/`export_name` symbol: `linked_name`.
   - A PGO record's name.

   This follows pureinval's `native-code-identity.md`, which keys code identity by a fact only
   when the code reads that fact.
3. **Positions are late-bound.** A panic location, a `Location::caller()` and a line row are a
   `SpanRef { anchor, index }`, filled at link from the owner's span tables. The code names the
   *table and index*. The anchor, which says *whose* table, is bound per instance. Moving code
   changes the tables and not the code. Spans are the same problem as names, and the fix is the
   same.

## The scheme

There are two identities per compiled body, because they do two jobs:

- **Link identity** (`Described::code`, `Instance::key`, `InstanceKey`). This is what the link,
  `Target::Function`, `SymKey` and the ICF name a function by. Two bodies written alike in two
  places must stay two functions: each reports its own panic locations, and instances shared
  across crates must call the same callees. So it keeps the key of the body's store. Unchanged.
- **Template key** (`Described::template_code`, used by `dev_template_key` and
  `release_template_key` through `FnNode::template_instance`). This is what the stored and
  memoized template is kept under. It is the body's `hir_ty_share::code` with nothing else added:
  - no store key, item key or name;
  - each callee by its interface hash;
  - each position by table and index.

  On top of that it holds only what the compiled body reads of names, `Hashes::names_read`,
  which `described` computes once and folds (`with_names`) into the link code too, so every
  key of the body holds it, owned or name-free:
  - the names of the files its `file!()` reads (`hir_ty_mir::mir::location::files_read`),
    folded into `built_in_code` as well, since a copy built into a caller bakes them in too;
  - where a debugger is told of its locals, `code::told_names`: what `side::Told::ty` (each
    named local's type identity) reads beyond the instance's arguments, spelled arm for arm as
    `identity::ty` does: each ADT by `identity::adt` (key, interface, layout and the `Drop`
    impls its fields reach), each foreign type, function item, closure and trait object by its
    item keys, and each coroutine by its key and `code::coroutine_names`. Each binding is
    hashed by its index, in binding order, with its own type's parts, so which binding has
    which type counts. This closes a gap the audit found. A told local whose type holds an alias
    (a projection, an opaque type, a free alias, an unevaluated constant) normalizes to
    something neither the code nor the arguments key, so that body's template is kept under
    its code (`names_read` says it is not whole).

The template holds the instance's own positions **relative**:
- Each `SpanRef` whose anchor is one of the instance's own span tables is rewritten to
  `template::own_anchor(table)`, a content ID of its own domain that anchors no real table.
- The rewrite applies to code relocations, data relocations and line rows.
- `Template::instantiate_at(key, refs, anchors)` writes the instance's own anchors back.
- A relative anchor with no anchors given is a typed refusal (`TemplateError::OwnPosition`).
- The template's encoding is unchanged: a `SpanRef` is still an anchor and an index.
  `TEMPLATE_DOMAIN` changes to "blaze v2 template 5", but it feeds only `Template::id()`,
  not any store key. Old store entries are invalidated by the code hash's domain bump
  (`CODE_DOMAIN`, "blaze v2 body code 2") and the new `with_names` fold, which change every
  instance and template key.

Two items whose bodies are written alike, before or after a rename or a move, share one template:

- one store entry;
- two instances, each instantiated with its own anchors and so its own positions.

The memo is a second matter. It keys a template by its IR's shape, and the IR still holds each
position's absolute anchor, so two such items are two memo entries of one template (their
store entry is shared). A relative and an absolute template of one IR must never share a memo
entry either: every prepare goes through the driver's `prepared`, which makes the shape
`Shape::relative` when the node has anchors. A body whose template key turns name-free, or back
(a report or profiling starts or stops naming it), then never takes the other form from a memo
kept across builds. A follow-up can hash the IR with relative anchors to share memo entries too.

Within one build, `Out::finish` refuses two puts of different bytes under one key
(`put_twice`), and `Out::keyed` does the same against the memo for the release, folded and
thunk templates. So two items that share a template key but compile to different bytes in one
build are refused, naming both. Neither check sees across builds: a key that misses a name is
caught only when both forms are built in one build, or by BLAZE_VERIFY_STORE.

### When the template code is the code

`template_code` is `code`, and `anchors` is `None`, when anything in the template is anchored
outside the instance's own tables, names it, or is not its own body's:

- **MIR inlining** (`inlines_as`): inlined spans are of the callee's tables, and side-table call
  sites hold absolute anchors.
- **The library's bodies** (a crate whose origin is `Lang`): `Build::options` always MIR-inlines
  them, as shipped, whatever the unit's inliner. Their code is their callees' bodies too, which
  the template code (callees by interface) does not hold.
- **Sanitizers** (`Units::names`): SanitizerCoverage skips a `__sanitizer_*` symbol by name, and
  LLVM's sanitizers write descriptors that name what they guard.
- **PGO or a report** (`Units::names`): `pgo::name` puts the path in the profile record, and the
  report rows hold absolute positions.
- **Shims, `main`'s start, the allocator and `asm!`**: a shim's `Declaration(func)` anchor is keyed
  by the adapted function's item key.
- **A closure written in an anonymous constant**: it has no tables of its own.

Bodies built in at -O0 (`#[inline(always)]` of crates no edit reaches) keep their own absolute
anchors. Their instance keys are already in the template key (`stored`).

## What stays name-keyed, and why

- **Nominal type identity** (`hir_ty_share::identity`: `TypeId`, `type_name`, vtables, dyn
  dispatch). Two types of one shape and two names are two types to the program. This includes an
  instance's type arguments: a generic instantiated with a renamed type is a new instance until
  compile identity's name-free variant (follow-up 2).
- **Link identity**. See above.
- **Statics**: their address is significant.
- **Exported, extern and `#[no_mangle]` symbols**: the link reads them.
- **Debug info**: variable names are read by binding index at link, so they are in no key. Type
  identities are in `told_names`, which is folded into the link code too (`with_names`) wherever
  a debugger is told of locals, since the side table of the linked function holds them. A told
  local whose type holds an alias is not folded: the link code keeps its store key and
  arguments, and what an impl or an opaque's defining body makes of the alias is a known limit
  until compile identity spells normalized types (follow-up 2). File and line tables are
  late-bound.

## Symbols stay deterministic (rule 24)

The link names local functions by `InstanceKey`, and the symbol string is only a debug name
(`name_digest`). Both are functions of content plus the store key, with no hash-map order, no
address and no clock. A local symbol can therefore be named by its link identity's content hash:
- it is deterministic, and two builds of one tree give one name;
- the code hash stays name-free, because the name is not an input of the template, only of the
  instance that is linked.

Exported symbols keep their declared names. The template never holds its own symbol:
`Ref::Own` and `Ref::Counter` are filled from the instance key.

## Salsa backdating

The cutoff points are:

| Edit | Early cutoff today | After this change |
|---|---|---|
| Rename a local | `code::of_item` changed: names were hashed | `of_item` backdates; the template, link identity and side table all hit |
| Rename or move a fn, update callers | `ItemKey` changes, so `store_key` and every `InstanceKey` change | Callers' `of_item` backdates (callee by interface). The renamed fn's template hits; its instance is new and is instantiated from the stored template, with no compile |
| Rename a callee | Callers' code hash equal; instance keys equal | Same; nothing recompiles |
| Move code down a file | Tables change, code not | Same |

What still re-runs:
- Interned ids are allocated in interning order, and `InferenceResult` holds `FunctionId`s. So a
  callee rename re-runs its callers' inference and MIR, which then backdate at `of_item`.
- The AST id hashes the item's name. A renamed type changes the AST ids of all its impls, which
  are re-lowered.

Re-parsing only the changed files needs AST ids keyed by position among same-kind siblings, not
by name. That is follow-up 4. Everything after MIR hits today.

## What can't be name-free without breaking rule 4

- `TypeId` and `type_name` of a renamed type. The program can see the difference.
- `#[track_caller]` locations, `line!()`/`column!()` values in consts and promoteds, `file!()`,
  and `module_path!()`. They are values in the program.
- Panic messages that format positions. They are late-bound, but the linked function must point
  at its own table, so the link identity stays per body.
- Exported symbols and `#[link_name]`.
- Proc macros and derives that read identifiers. Their expansion is different code, which is
  keyed by its own content.

## Follow-ups

1. **CodeId** (`hir-ty-identity`, pureinval's). It holds `item_key` through DEF_FN and
   `instance_callee`. Port it to callees by interface and placement, as templates are. Then a
   generic instance is keyed by what its arguments' code reads, not their names.
2. **Compile identity's template variant.** Today it records callees by `item_key`, because
   shared instances must call the same callees for link identity. A template takes any callee by
   place, so a template variant can record each callee by its interface and placement only.
3. **Relative side-table anchors.** Inlined call sites and report rows would let MIR-inlined and
   reporting bodies use the template code as well.
4. **AST ids by position.** Ids keyed by position, not name, would let a rename re-parse only its
   file.
5. **Live-patch slots by content.** Key a slot by the body's template code and owner chain, so a
   rename is an edit of one slot, not a removal plus an addition.
6. **PGO by content.** Name a profile record by link identity, not path, and key it in
   `release_template_key`.
7. **Const-eval `Location::caller` in generic, inline-const and body-form cases.** Key it by
   position where it is read.
