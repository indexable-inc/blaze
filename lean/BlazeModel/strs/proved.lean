-- Hand-written, not generated: the proof of the `strs` fixture's statement.
import Blaze
import BlazeModel.strs.greeting_is_hello

/-!
`str` as its bytes, proved through its model (crates/hir-ty-lean/src/tests.rs, `STRS`;
lean/Blaze/Str.lean): `"hello"` has 5 bytes and equals the literal `"hello"`, either string
`pick` passes along is not empty, and `""` is empty. Every string here is a literal, so the
statement is decided by computing it.
-/

open Blaze

theorem strs.greeting_is_hello.spec.proof : strs.greeting_is_hello.spec.statement := by
  intro first
  cases first <;> decide
