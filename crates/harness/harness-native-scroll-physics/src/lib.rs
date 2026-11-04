//! macOS scroll physics, 1:1 with a default `NSScrollView`: AppKit's scroll animator (velocity filter, rubber band while
//! dragging, pixel alignment), the Cates momentum coast with its edge bounce, and the per-surface engine that drives
//! them from raw wheel events and display-link ticks.
//!
//! Copied from den (github.com/andrewgazelka/den, crates/den-scroll-physics), which copied it verbatim from the os GPUI
//! fork (`zed/crates/gpui/src/scroll_physics/`, 71f75df3c4), where each piece was
//! measured against the real AppKit classes; the oracle tables and traces stay in den, where they run as the tests. The design and
//! measurements are `os/research/scroll-physics.md`.
#![allow(dead_code)]

mod animator;
mod engine;
mod momentum;

pub use animator::{Phase, Vec2};
pub use engine::{Engine, Input, Mutation, SurfaceConfig};

impl Phase {
    /// An `NSEventPhase` (the same bit values).
    pub fn from_ns(bits: usize) -> Self {
        match bits {
            1 => Phase::Began,
            2 => Phase::Stationary,
            4 => Phase::Changed,
            8 => Phase::Ended,
            16 => Phase::Cancelled,
            32 => Phase::MayBegin,
            _ => Phase::None,
        }
    }
}
