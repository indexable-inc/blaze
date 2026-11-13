---
status: plan
summary: Error sets (edition change 2, ledger row E3). A function's error type is the exact set of failures it may return, written `A | B` under `#![blaze::error_unions]`. `?` widens a member or a smaller set into a larger one, a set the body's failures do not fit is an error naming the call, the member and the set, a `match` covers every member, and the set costs what the hand-written enum costs. What is built, its floor, and the list of bstd signatures that move onto it.
date: 2026-11-12
---
# Error sets

Asked for by Andrew, 2026-11-12 (decision D30): a function's error type says exactly which
failures it can return, the way `#[blaze::may(..)]` says exactly which effects it has
([effects](effects.md)).

```rust
#![blaze::error_unions]

fn main(cx: &Cx, io: Io) -> Result<(), Missing | FsError> {
    let fs = io.need_fs()?;                      // Missing
    let data = fs.open_dir(cx, path!("data"))?;  // FsError
    ..
}

match main(cx, io) {
    Ok(()) => {}
    Err(Missing { capability }) => ..,
    Err(FsError { kind: FsKind::NotFound, path, .. }) => ..,
    Err(e @ FsError { .. }) => report(e),        // e: FsError
}
```

No enum is declared, no `From` is written, and no arm is `_`. This is edition change 2
([edition](edition.md), "Error unions"); "error set" is what a union is when it is a function's
error type, and the type keeps the name error union in the code.

## The parts

| part | what | where | built |
|---|---|---|---|
| the type | `A \| B` is core's `ErrorUnion<(A, B)>`, its members flattened, deduplicated and in one canonical order, so `A \| B`, `B \| A` and `A \| (B \| A)` are one type in every crate | `crates/hir-ty/src/lower/error_union.rs`, `library/core/src/error_union.rs` | E3 part 1 (92ea33709b) |
| widening | `U: UnionFrom<S>` holds when `S` is a member of `U` or a set of some of its members; only the solver answers it, with the path each value takes; core's `Inject` impls over those paths are the conversion, a blanket `From<S> for U` over both makes `?` widen, and a coercion (`Err(e)`, a return, an argument, a typed `let`) calls `widen` | `vendor/rustc_next_trait_solver/src/solve/error_union.rs`, `crates/hir-ty/src/infer/coerce.rs` | E3 part 2 (eb4c66b4c3) |
| `Result<T, A \| B>` parses | a set whose first member starts with a name was a syntax error as a generic argument | `crates/parser/src/grammar/generic_args.rs` | this lane |
| matching | a constructor pattern of a member matches the set by member | `PatAdjust::UnionMember`, `crates/hir-ty/src/error_union.rs` | this lane |
| the check's words | a `?` whose failure the set lacks is said as that, with the fix | `crates/blaze-diagnostics/src/lower/unimplemented_trait.rs` | this lane |

**Library over language** (rule `library-over-language`). The conversions, the delegated traits
(`Debug`, `Display`, `Error`, `Clone`, `Hash`) and the representation are library code in
`core::error_union`. The compiler holds three primitives no library can express: the type
syntax with its canonical order (a library cannot sort types), `UnionFrom` (set membership
answered from the types alone; as impls it would overlap `impl<T> From<T> for T`), and the
member pattern below. Members are concrete named types, 2 to 12 of them; a type parameter is
refused (with two parameters equal, `T | U` would have one member or two).

## The check

A signature's set is a positive list, as an effect list is: what it leaves out cannot happen.

- **Too little is an error.** `?` on a call whose failure is not in the set does not type-check
  (`From` does not hold). In a crate with error unions it is said as what it is, at the call:

  ```text
  error[E0277]: `fs.open_dir()?` can fail with `FsError`, which the error set of `run`, `Missing`, does not have
  14 | pub fn run() -> Result<u8, Missing> {
     |                            ------- the error set of `run`
  16 |     let dir = fs.open_dir()?;
     |               ^^^^^^^^^^^^^^ fails with `FsError`
  help: add `FsError` to the set, or handle it at the call
  14 | pub fn run() -> Result<u8, Missing | FsError> {
  ```

  The function's error type need not be a union yet: one type is a set of one. A call that
  fails with a set names only the members missing.
- **Too much is advice**, not built: a member of a private function's set that no `?`, `Err` or
  return of its body can produce (`blaze::unneeded_member`, `warn`, with the fix that takes it
  off), as `unneeded_effect` does for a word. A `pub` function may reserve a member.
- **Inferred sets** (Zig's `!T`), not built: a private function writes `Result<T, _>` and the set
  is the union of what its body returns. It needs the set as an inference result the signature
  reads, which is the cycle Zig also has for recursion; the border rule of the edition (infer
  inside, declare at `pub`) is where it goes.

## Matching

A struct, tuple-struct or path pattern whose type is one member of the set it is matched
against matches when the set holds that member, against the member's value: `FsError { kind, ..
}`, `Stopped::Cancelled`, `Wrap(x)`. An enum member is matched by its variants. References are
peeled first, as for any constructor pattern (`&(A | B)` binds by reference). `x @ FsError { ..
}` binds the member, not the set (a `&FsError` through a reference), when the pattern after
`@` names the member's struct or variant by path. Only in a crate with the attribute; a
pattern of no member is the type mismatch it was.

Inference records one adjustment on the pattern, `PatAdjust::UnionMember`, whose source is the
set; which member is the pattern's own type (under `x @`, the type of the pattern after it),
so nothing else is stored (InferenceRecord schema 31). Every reader of pattern adjustments turns it into the pattern it stands for, core's
`ErrorUnion` struct around the variant of its `Repr` enum that holds the member
(`hir_ty::error_union`): match checking (so a match is exhaustive exactly when every member is
covered, a member left out is the witness, written `FsError { .. }` or `Stopped::_`, and a member
covered twice is unreachable), MIR lowering (a discriminant test, then the variant's field),
the match tree and closure capture analysis.

Not built: the typed binding of the edition page, `Err(e: FsError)` and `_: Stopped`, which
binds or tests a member whole without naming its fields or variants (today `e @ FsError { .. }`
for a struct; an enum member whole has no pattern). It is sugar for the same adjustment and
needs a pattern form in the parser and `hir-def`. Narrowing the rest of a set after an arm
(`E is not FsError`) waits for pattern types.

## Representation and cost

A set is `ErrorUnion<M>`, a struct of one field, core's enum `OfN` with one variant per member
in canonical order. The order is by each member's identity, a BLAKE3 hash of its crate's
content ID (the crate's configuration and dependencies, not its source), its module path, its
name and its arguments: no interned ID, address or map order (rule `deterministic-output`).

Floor (rule `within-ten-percent-of-the-floor`): the enum a person writes by hand for the same
members, with a `From` impl per member.

| operation | floor | a set | measured |
|---|---|---|---|
| size and alignment | the hand-written enum's | the same: one enum under a one-field struct | equal for 2 and 3 members, bare and as a `Result`'s error; `Missing \| FsError` is 32 bytes, as is `Result<u8, Missing \| FsError>` (`layout::error_unions`) |
| a member into a set | write the tag, move the payload | `From::from`, `widen`, `Inject::inject`, each `#[inline]`, around the same construction | not run through codegen in this lane: at a tier that does not inline, three calls on the failure path where the hand-written `From` is one |
| a set into a larger set | a match on the tag, each arm the line above | the same match, written once in core per arity | as above |
| a match on a set | a switch on the tag | the same switch | evaluated in MIR at each optimization level (`mir::error_unions`) |
| a crate without unions | nothing | one comparison of an item's ID per constructor pattern, one more blanket candidate per `From` goal (refused at its first test), nothing in the parser | not measured |
| adding a member | the signature's type changed: its direct readers rerun | the same; other members' variants may move, in code that reruns anyway | not measured |

What no design here reaches: a rename or a move of a member's item changes its identity, so
the sets that hold it may be reordered and their users recompiled (ledger T4 does not hold for
a member). Ordering by an identity that survives a rename
([name-free-identity](name-free-identity.md)) is the way out and is not built.

Losing alternatives, each in a line:

- **A named enum with derived `From`s** (`thiserror`'s shape, or a reflection-derived impl): every
  distinct set needs a declaration and a name, and a set of sets needs a second layer; a
  `From` over every smaller set cannot be written beside `impl<T> From<T> for T`.
- **Zig's global error codes** (a set is an integer, widening is free): members carry no
  payload, where bstd's errors name the operation and the thing (rule
  `errors-name-the-thing`).
- **A library coproduct** (`Or<A, Or<B, Never>>`, frunk's shape): widening needs an index
  argument inference must find, which `From` has no place for; nested tags; `match` is nested.

## bstd on error sets

bstd's functions already return one area error each, and a caller that mixes two gets the set
for free. What the port changes is the sums bstd writes by hand today: a kind variant that only
holds another error. At 136432aa02, 43 lines outside bstd's tests name such a `Stopped`
variant, and three `From` impls exist to fill them. The port waits for D29 (fallible everything), which is changing these
signatures; the list is by rule so it can be read against whatever D29 lands.

One line in `crates/blaze-std/src/lib.rs`: `#![blaze::error_unions]` among the inner attributes
that open the root.

| today | becomes | signatures (file:line at 136432aa02) |
|---|---|---|
| `FsKind::Stopped(Stopped)` in `FsError` | `Result<T, FsError \| Stopped>`; the variant goes | every `pub fn` of `fs.rs` that takes a `Cx` and returns `FsError`: `Fs::open_dir` 684, `Dir::{list 796, stat 852, open_read 876, read_all 883, open 890 and 898, create 909, remove 1050, remove_unsynced 1059, remove_synced 1067, rename_replacing 1077, rename_replacing_unsynced 1087, rename_replacing_synced 1106, sync_names 1118}`, `Written::flush` 1184, `Flushed::clone_to` 1253, `File::{len 1329, read_into 1338, read_at 1358, read_all 1377, flush 1511, close 1522, close_synced 1545}` |
| `FsKind::OverQuota(Over)` too | `Result<T, FsError \| Stopped \| Over>`; both variants go | the writes that pass a quota (`within`, `Budget::take`): `Dir::{publish 991, publish_unsynced 1015, publish_synced 1035}`, `copy_to` 1203 and 1262, `File::{append 1403, append_unsynced 1412, append_synced 1420, write_from 1427, write_at 1460, write_at_unsynced 1469, write_at_synced 1478, set_len 1485, set_len_unsynced 1493, set_len_synced 1502}`, `Buffered::{push 1619, write_out 1643, finish 1656}` (they append) |
| a failed reserve dressed as `FsError` | `Result<Buffered, Stopped>` | `File::buffered` 1594 (it takes no `Cx` and touches no file) |
| `NetKind::Stopped(Stopped)` in `NetError` | `Result<T, NetError \| Stopped>` | `net.rs`: `Net::{connect 503, listen 521}`, `Listener::{accept 565, close 586}`, `Conn::finish` 771; `resolver.rs`: `Resolver::lookup` 44; `local.rs`: `Local::{connect 286, listen 295}` |
| `NetKind::OverLimit(Over)` too | `Result<T, NetError \| Stopped \| Over>` | `Conn::{write_from 661, send_all 694, read_into 705, recv 750}` |
| `LocalKind::Stopped(Stopped)` in `LocalError` | `Result<T, LocalError \| Stopped>`; `LocalKind` is left with `Closed` and goes | `local.rs`: `End::{write_all 171, read 203}` |
| `SleepError { Stopped(Stopped), Driver { .. } }` and `From<Stopped> for SleepError` | `Result<(), Stopped \| ClockDriver>`, `ClockDriver` the struct the `Driver` variant was; the enum and the impl go | `clock.rs`: `Clock::{sleep_until 91, sleep 104}` |
| `HttpKind::{Net(NetError), Stopped(Stopped)}` in `HttpError` and `From<Stopped> for HttpKind` | `Result<T, HttpError \| NetError \| Stopped>`; the variants and the impl go | `requests.rs`: `Http::{begin 315, request 368}`, `Exchange::finish` 408 |
| `BlobsKind::{Store(KvError), Stopped(Stopped)}` in `BlobsError` | `Result<T, BlobsError \| KvError \| Stopped>`; `BlobsKind` is left with `Corrupt` | `blobs.rs`: `Blobs::{get 157, has 172, list 179, put 196, put_unsynced 205, put_synced 215, delete 225, delete_unsynced 233, delete_synced 240}` |
| `Refused::Stopped(Stopped)` and `From<Stopped> for Refused` | `hosted::{call, wait}` return `Refused \| Stopped`; each area's function returns `XError \| Stopped`; the variant and the impl go | every `pub fn` that reaches `hosted::call` or `hosted::wait` and returns `AudioError`, `AutomationError`, `CodeError`, `DeviceError`, `KeysError`, `OpenerError`, `PowerError`, `ProcsError`, `StreamsError`, `TermError` or `WindowError` (`audio.rs`, `automation.rs`, `code.rs`, `device.rs`, `keys.rs`, `opener.rs`, `power.rs`, `procs.rs`, `streams.rs`, `term.rs`, `window.rs`) |
| `KvError { under: FsError }` | stays: it adds the key and the operation to the file's error, which is context, not a sum; its `under` follows `FsError`, so `Kv`'s nine functions (kv.rs 230 to 314) return `KvError \| Stopped` | |
| `TaskWhy::OutOfMemory(Stopped)` | `Scope::spawn`'s reserve failure is the `Stopped` it is: `Task::join` returns `TaskError \| Stopped` | `task.rs` 221, 322 |

Stay as they are: `Io::need_*` (`Missing`, a set of one, which is what makes `main` above
work), the name types' errors (`PathError`, `UrlError`, `KeyError`, `NameError`,
`HostPortError`, `VarNameError`, `HeaderError`), `ImageError`, `VideoError`, `MuxError`, and
`TaskWhy::Failed(String)`: a task's own error `E` as a member needs a set over a type
parameter, which is refused.

What the port decides and a reviewer should check: a `Stopped` no longer carries the file or
the peer it stopped on (`FsError { op, path, kind: Stopped }` did); its `span` names the wait.
A deadline is about the task, not the file, and one `Err(Stopped::Cancelled { .. })` arm then
handles every area's cancellation.

## Proof

- `crates/hir-ty-mir/src/tests/mir/error_unions.rs` runs programs in the MIR evaluator at each
  optimization level, on errors shaped as bstd's: `?` widens `Missing` and `FsError` into one
  set and each failure comes out as its member with its fields; a smaller set widens; an enum
  member is matched by its variants; members match through a reference, under `@`, in `let`-
  `else` and `if let`, and in a closure that captures the set.
- `crates/blaze-diagnostics/src/tests/bodies/error_unions.rs`: the missing member's error, word
  for word, with its control (the member added, no error); E0004 naming the member.
- `crates/hir-ty/src/tests/error_unions.rs`: the invariant that a set's members have one order
  in every crate and every build, with its control (an order by the database's numbering,
  on which two builds that declare the members in opposite orders disagree); the adjustments
  inference records; a crate without the attribute.
- `crates/hir-ty-mir/src/tests/layout/error_unions.rs`: a set's layout is the hand-written
  enum's, and the same in every order it is written in.
- `crates/hir-ty-matchck/src/tests.rs`, `a_match_on_an_error_union_is_exhaustive_over_its_members`.

Not proven here: a blaze-built program (these run blaze's front end and evaluator under cargo;
no code was generated), and bstd itself, which the port will be the proof of.
