# tools/std-map.json

What each item of Rust's `std` and `alloc` that blaze's crates use becomes on bstd and
blaze-own: one table, read by the `ambient` ratchet's rules, the `.clippy.toml` bans and the
`tobstd` translator (`scripts/src/bin/tobstd.rs`). JSON with no comments; this file is its
schema. Rows are sorted by `sites`, most first (a row with `sites: null` last), then by `written`.

The top level is an object:

| field | type | what |
|---|---|---|
| `version` | integer | 1; a change of a field's meaning raises it |
| `counted` | string | how `sites` was counted, and when |
| `rows` | array of row | one per item, as written in the tree |

A row:

| field | type | what |
|---|---|---|
| `path` | string | the item's canonical path (`alloc::vec::Vec`, `core::mem::take`, `std::fs::read`) |
| `written` | string | how the tree writes it: the path, a prelude name (`Vec`, `Vec::new`), a macro (`format!`), a method call (`.to_owned()`), or a receiver and method (`"..".to_string()`) |
| `form` | `"path"`, `"prelude"`, `"macro"` or `"method"` | which of those `written` is |
| `kind` | `"type"`, `"function"`, `"macro"`, `"trait"`, `"method"` or `"module"` | what the item is |
| `sites` | integer or null | sites under crates/ outside test code (`counted` says how); null for a row kept because `.clippy.toml` names it or because the scan cannot count it |
| `replacement` | string or null | the bstd, blaze-own, blaze-hash, blaze-core or `core` path written instead, with the call's shape where it changes (`Vec::new_in(alloc)`); null when there is none |
| `needs` | array of string | what the replacement needs that the original did not: `alloc` (an allocator value), `cx` (the task's `Cx`), or a capability by its type's name (`Fs`, `Net`, `Clock`, `Env`, `Rng`, `Args`, `Out`, `Procs`); empty for nothing |
| `class` | string | one of the five below |
| `note` | string or null | what the class leaves to say: what code does instead for `no-form`, what bstd lacks for `missing`, where `.clippy.toml` disagrees |

The classes:

| class | the change | who makes it |
|---|---|---|
| `rename` | the same shape at another path (`std::mem::swap` to `core::mem::swap`) | mechanical |
| `add-arg` | the same call or type plus an allocator the caller supplies (`Vec::new()` to `Vec::new_in(alloc)`, `format!` to `format_in!(alloc, ..)`, `Vec<T>` to `Vec<T, A>`) | mechanical where an allocator is in scope; else the translator threads it |
| `thread` | needs a value that must come down from `main`: a capability or `Cx` | the translator's loop, with the compiler's errors |
| `no-form` | no bstd form by design (`Default` for a collection, `FromIterator`, `to_string`, `thread_local!`, a `static` with interior mutability) | a person, by `note` |
| `missing` | bstd or blaze-own lacks it | the crate that should have it, by `note` |

A row whose `note` begins with or holds `.clippy.toml` agrees with that ban's replacement or
says where it does not.
