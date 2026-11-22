//! C and C++ translation units as code records (docs/v2/plan/design/c.md, part 3 and "Order" step
//! 4; D174, D176): a unit is compiled by the pinned clang to a relocatable object, and that object
//! is cut into one record a function and one a data object, so a C function is placed and called
//! as a Rust body is, with no link step.
//!
//! | module | what |
//! |---|---|
//! | [`key`] | the unit key (language, path, source digest, headers read, flags, target, toolchain), and each atom's record key and unit-local symbol from it |
//! | [`split`] | the object, read by blaze-native, cut into atoms: symbol, bytes, alignment, unwind row, and its relocations as references to other symbols BY NAME (or to an atom of the unit); the unit's init and term rows |
//! | [`lower`] | an atom made a blaze-code code record (a function) or datum (a data object), the form Rust bodies are made in; what the form cannot hold yet is a typed refusal naming it |
//! | [`index`] | the symbol index record: each C name to the unit that defines it, from the units' declarations alone; two strong definitions are a refusal naming both units, weak and COMDAT ones the least unit key |
//!
//! **Why objects and not bitcode** (c.md part 3's choice): clang's object is the C compiler's
//! finished answer, including the parts no other front end makes (C++ EH tables, inline asm,
//! every intrinsic, the target's ABI lowering); cutting it at symbols is what every linker does
//! (ld64's atoms), and the cut is a function of the bytes alone. Bitcode through blaze's emitter
//! would have bought cross-language inlining and records shaped as blaze-emit shapes them (no
//! veneers, a frame word blaze's unwinder reads), at the price of translating all of LLVM IR into
//! LIR or running LLVM's code generator inside blaze with a record-writing back end: work the size
//! of a second compiler, before the first C function runs. The tier-up (D-tier) may take bitcode
//! later for hot C functions, behind the same record key.
//!
//! **Initializers** (C++ static constructors, `__attribute__((constructor))`): the unit's
//! [`split::Init`] rows. THE RULE: every init row of every unit the run includes is called before
//! the program's entry, ordered by (priority ascending, then the units in the order their
//! package's `sources` list them after expanding and sorting the glob by path, packages in
//! dependency order, dependencies first, then the row's order in its unit); term rows are called
//! after the entry returns in the exact reverse order. A unit with init rows is included when the
//! program's C-name roots pull it by the index (an archive member's rule: an unreached unit's
//! constructors do not run, as an unreferenced archive member's do not), decided before the
//! entry, never on first call.
//!
//! Sans-IO: bytes in, records out. Nothing here runs clang, reads a file or places code.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod index;
pub mod key;
pub mod lower;
pub mod split;

#[cfg(test)]
mod tests;
