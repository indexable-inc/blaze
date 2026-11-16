//! The Miri control, variant `NOTHING`: `dealloc` writes nothing; the block is handed out again
//! at once and its new owner writes it (see `toy/mod.rs`).

#[macro_use]
#[path = "toy/mod.rs"]
mod toy;

#[global_allocator]
static TOY: toy::Toy<{ toy::NOTHING }> = toy::Toy::new();

cases!();
