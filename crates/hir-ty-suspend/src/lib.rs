//! Values held across a call that may park a fiber (docs/v2/plan/design/may-suspend.md).

#![forbid(unsafe_code)]
#![feature(allocator_api)]

mod check;
mod dispatch;
mod root;
mod summary;
#[cfg(test)]
mod tests;

pub use check::{
    Broken, Found, Held, HeldLocal, Point, Promise, Site, Unchecked, Why, may_suspend_at, on_fiber,
    suspend_check, suspend_points, unchecked,
};
pub use dispatch::Unseen;
pub use root::MissingRoot;
