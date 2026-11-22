//! The Miri control, variant `OWN`: the link written into a freed block through our provenance
//! (see `toy/mod.rs`).

#[macro_use]
#[path = "toy/mod.rs"]
mod toy;

#[global_allocator]
static TOY: toy::Toy<{ toy::OWN }> = toy::Toy::new();

cases!();
