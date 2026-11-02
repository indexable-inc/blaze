//! Values held across a call that may park a fiber (docs/v2/plan/lanes/may-suspend.md).

#![forbid(unsafe_code)]

mod check;
mod dispatch;
mod root;
mod summary;
#[cfg(test)]
mod tests;

pub use check::{
    Broken, Found, Held, HeldLocal, Point, Promise, Site, Why, may_suspend_at, on_fiber, suspend_check, suspend_points,
};
pub use dispatch::Unseen;
