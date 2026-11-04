//! hyperion-layout: layout recipes and the layout hash (Zig `plugin.layoutHash`). One job: describe a type's layout and know its
//! hash. Trimmed from hyperion for stable Rust: the recipes are written by den-flecs-derive (`offset_of!`, `size_of!`) instead of
//! the hyperion compiler's `Shape::of`; migration chains, persist specs and host memory types are left out.
#![no_std]

extern crate alloc;

pub mod layout;

pub use layout::{Entity, Layout, Recipe};
