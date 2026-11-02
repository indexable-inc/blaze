---
status: plan
summary: dedup-scan-ident (2026-10-29): one spelling of a type, constant and argument list by content (hir_ty_share::identity), one body code with spans (hir_ty_share::code) and one is_plain_data, for both hir-ty-codegen and hir-ty-share; why the coroutine arm follows the code generator's; what stays apart on purpose; speed of light.
date: 2026-10-29
---
# dedup: code identity

Part of [dedup](../dedup.md), under `dedup-scan`. The edits are requests, because neither
owner is free:

- `v2/requests/fastcompile-ident-share.md`, `## dedup: one type identity for the code
  generator and the share key`;
- `v2/requests/codegen.md`, `## dedup: one type identity (hashes.rs onto hir-ty-share)`.

## The copies

| what | copy A | copy B | outcome |
|---|---|---|---|
| a type, constant or argument list by content | `hir-ty-codegen/v2/hashes.rs` `Spell`, `adt_content`, `variants` | `hir-ty-share/src/identity.rs` `Spell`, `adt`, `variants` | one: `hir_ty_share::identity` |
| a body's code with its names and positions | `hir-ty-codegen/v2/hashes.rs` `with_spans`, `names`, `table_of`, `*_code_in` | (none; share needed it and lacked it) | moved verbatim: `hir_ty_share::code` |
| whether a type is plain data | `hir-ty-codegen/mod.rs` `FunctionCx::is_plain_data` | `hir-ty-share/src/fact.rs` `Cx::is_plain_data` | one: `hir_ty_share::is_plain_data` |
| a little-endian `std::hash::Hasher` feed | `hir-ty-share/src/layout.rs` `Feed` | `blaze-hashcons/src/std_hash.rs` `Writer` (and `v2/fold.rs` `Le`, others) | sibling `dedup-codec`'s, told |
| fixture lookups `declarations`, `function`, `alias` | `hir-ty-codegen/v2/hashes/tests.rs` | `hir-ty-share/src/tests/mod.rs`, `v2/refs/tests.rs`, `hir-ty-mir/tests/mir/main.rs` | `dedup-scan`'s test-database request |
| tag constants | `v2/shape.rs` `part` | `walk.rs` and `fact.rs` `tag`, `blaze-link` wasm `id` | not copies: separate schemas numbered 1 to N |

Not copies, though they walk types too:

- `blaze-items/src/form.rs`: the code and interface hash grammar. It names items by key,
  keeps regions, and records misses instead of failing.
- `hir-ty-codegen/type_id.rs`: rustc's `TypeId` stable hash, which rustc's code compares with.
- `stable.rs` and `fncache.rs`: v1 symbol names and the function cache.
- `v2/frontend.rs` `args_of`: a fold over the `Hashes` seam, which the tests fake.

## Why the one spelling is the code generator's, in `hir-ty-share`

The two `Spell`s differ in buffering, string framing, the `FnPtr` ABI text, domains and the
error type. None of these changes which types hash equal. They also differ in one arm that does:

- **Code generator:** a coroutine (and its witness) is spelled by its key, its compiled code
  (code hash, binding names, positions, and for a by-move body the closure's captures) and its
  arguments.
- **Share:** a coroutine is spelled by its key, `blaze_items::coroutine_code` and its arguments.

Within one revision the two agree, since one coroutine key is one body. Across an edit they do
not: renaming a binding inside an `async` block changes the first and not the second.

The code generator's arm is deliberate. `Hashes::ty` is three things:

- the instance's argument key;
- the `TypeId` value (`fill_type_ids` writes its first 16 bytes);
- the debug-info type key (`side::Variable::ty`), where a coroutine's saved locals are named by
  their bindings.

So once the share key goes into `Described::args` (fastcompile request 1), share's arm becomes
a cache key that misses an input. A template for `tid::<C>`, or for anything holding a `C`,
would outlive an edit of `C`'s body and keep a stale `TypeId` and stale debug types. Finer is
always safe for a key; coarser is a miscompile. So the one spelling takes the code generator's
arm.

That arm reads the body code with spans, so that code moves into the same crate. It moves
verbatim, so `Hashes::code` does not change by a bit.

The home is `hir-ty-share`:

- it is a front-end salsa crate (AGENTS.md rule 8);
- codegen depends on it anyway for the share key;
- its copy is the newer one (`&Db`, the new `rustc_type_ir`, streaming into the hasher).

`blaze-items`, the code-identity crate, would be at least as good a home. It is not free:
`mem-backend-items` is moving it onto `blaze-mem`. A later move from `hir-ty-share` to
`blaze-items` is one `git mv` plus imports, because the new modules depend on nothing of
`hir-ty-share`'s but `Unshared`.

Domains: the spelling's bytes change for both old callers (the `FnPtr` text for codegen, the
coroutine arm for share), so the one spelling gets new domains. Types, constants, argument lists
and trait-object bounds each get their own (`blaze v2 identity type 1`, `const 1`, `args 1`,
`bound 1`, `adt 1`); share's old one domain served three grammars. `hir_ty_share::code` keeps
codegen's domains, because its bytes are unchanged.

## The proof

`hir-ty-share/src/tests/identity.rs` (in the request) keeps both old spellings as the spec
(`OldSpell` with `Old::{Codegen, Share}`, and the code generator's `is_plain_data`). It checks
the following:

- **Agreement within a revision.** Over a fixed corpus (every `TyKind` arm a fixture can name;
  closures, coroutines and parameters from MIR locals; opaque types for the error path), and
  over 32 proptest cases of random types each written under two names, the one spelling fails on
  exactly the types each old one fails on, and tells apart exactly the pairs each old one tells
  apart. Constants and argument lists are checked the same way.
- **The deliberate difference, across an edit.** After a binding inside an `async` block is
  renamed, the one spelling and codegen's old one change, and share's old one does not. The
  test pins the missed input.
- **`is_plain_data`.** It equals the code generator's old copy on every corpus type.

`hir-ty-codegen/v2/hashes/tests.rs` keeps its tests unchanged. They now run over the one
spelling through `ItemHashes`.

## Speed of light

**The identity of a type** spells \(S\) bytes into BLAKE3 and finalizes once. The work is
\(\lceil (S+1)/64 \rceil\) compressions. One BLAKE3 compression of a lone block is about 60 to
80 cycles: 7 rounds of 8 G functions, and no SIMD across chunks, since a spelling is less than
one 1 KiB chunk. An instance argument such as `Vec<u8>` or `&'static P` spells to 45 to 60 bytes:

- 1 to 2 compressions, about 25 to 50 ns at 3 GHz;
- one salsa memo hit for the ADT, about 20 ns.

So the bound is about **50 to 70 ns** per argument.

- Codegen's old `Vec` copy paid one `malloc`/`free` of 256 bytes, about 20 to 40 ns, plus the
  pushes.
- Share's streaming copy pays the per-call cost of `blake3::Hasher::update` instead, about 3 to
  5 ns for each of roughly 10 updates of 1 to 32 bytes.

Both land near 1.5× the bound. That is more than 10% off, so a bug by the rules.

What reaches the bound is an inline buffer of a few hundred bytes in `blaze_ids::Hasher`
itself, flushed to BLAKE3 when full. Every tiny-update caller gains from it, not only this one:

- `fact.rs`, `walk.rs` and `layout.rs`'s `Feed`;
- the items lane's hashes.

That is a hasher primitive, owned by sibling `dedup-codec` (told). It changes no hash, since
BLAKE3 is independent of how its input is split. With it the spelling is pushes into L1 plus the
compressions: within 10%.

Past that, the next step is not hashing at all on a hit. Memoizing the identity per interned
type (a tracked query on `StoredTy`) makes a repeat argument one memo lookup, about 20 ns. It
costs one memo per distinct type, and nested types become Merkle children (their ids instead of
their bytes). That is a change of grammar, so it is for the owner to weigh against memory.

**The body code** (`hir_ty_share::code`) is tracked, so a hit is one memo lookup. A miss hashes
\(1 + 32p + 5k + 9b\) bytes, for \(p\) parts, \(k\) spans and \(b\) bindings. That is
\(\lceil n/1024 \rceil\) chunks, which BLAKE3 hashes several at a time with SIMD on a single
large `update`. One buffer and one `update` is the fast shape for this size, so `with_spans`
keeps its one buffer (`// alloc:` on a memoized miss). `names` streams, since its updates are
few and it is small.

**`is_plain_data`** visits at most \(\sum_{d \le 8}\) the fields of the type to depth 8. That
is one normalize check (an `Alias` test, nothing for a normalized type) and one `field_types`
memo hit per ADT. No allocation (the old codegen copy collected each enum's variants into a
`Vec`; the one iterates them). The work is the visit, so the bound is the visit.
