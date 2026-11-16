//! The Miri control, variant `CALLER`: the link written through the pointer `dealloc` was given,
//! read back through ours (see `toy/mod.rs`).

#[macro_use]
#[path = "toy/mod.rs"]
mod toy;

#[global_allocator]
static TOY: toy::Toy<{ toy::CALLER }> = toy::Toy::new();

cases!();
