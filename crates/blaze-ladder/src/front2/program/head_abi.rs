//! The ABI a fn is declared with, from its HEAD alone: one home for the rule, read by the
//! table's rows (tabled.rs `row`) and by the builder's question for a fn the program does not
//! table ([`FrontEnd::abi`] on [`Front2Program`]).
//!
//! An ABI is a fact of a fn's signature, never of its body: the Reference, "Functions":
//! "Functions may qualify their signatures with an ABI: `extern "C" fn ..`", and "External
//! blocks": "Functions within external blocks ... the ABI of the block". So a fn whose body
//! the program leaves out (a provided method whose default body does not check, called only
//! through a vtable) still has its ABI, and a dynamic call through `&dyn Tr`, which names the
//! trait's fn and never its body (the Reference, "Trait objects"), is built with it
//! (tests/front2_callee_left_out.rs `a_dynamic_call_needs_no_body_of_the_method`).

use super::*;

/// The ABI fn `key`'s head declares, as the builder spreads arguments: Rust, C or rust-call.
/// A foreign fn's own text carries no ABI: its block's does (a module-level block's from the
/// names library, a block in a body's from the store: front2/panic_entry.rs `local_abi`). A
/// constructor is a safe Rust fn. Refused by name: no head ([`Front2Refusal::NoHead`]), an ABI
/// the builder spreads no arguments of ([`Front2Refusal::Abi`]). `function` names the fn in
/// the refusal.
pub(super) fn head_abi<'n>(names: &Names<'n>, store: &Store<'n>, key: ItemKey, function: impl Fn() -> Box<str>) -> Result<Abi, Front2Refusal> {
    if store.ctors.get(&key).is_some() {
        return Ok(Abi::Rust);
    }
    let Some(head) = store.fns.get(&key) else {
        return Err(Front2Refusal::NoHead { function: function() });
    };
    let abi = match head.header.abi {
        tc::Abi::Rust => Abi::Rust,
        tc::Abi::C => Abi::C,
        tc::Abi::RustCall => Abi::RustCall,
        // The builder spreads no other ABI's arguments.
        abi => return Err(Front2Refusal::Abi { function: function(), abi }),
    };
    let (_, foreign) = placed(names, head.lib);
    let foreign = foreign.or_else(|| crate::front2::panic_entry::local_abi(store, key));
    match foreign {
        None => Ok(abi),
        Some("C" | "C-unwind") => Ok(Abi::C),
        Some("Rust") => Ok(Abi::Rust),
        Some("rust-call") => Ok(Abi::RustCall),
        Some(_) => Err(Front2Refusal::Abi { function: function(), abi: head.header.abi }),
    }
}
