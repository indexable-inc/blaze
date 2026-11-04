//! blaze's dev runtime built in blaze's workspace: library/blaze_inproc_rt's source, the one
//! the standard library's build links into every dev executable (its own docs say what it is).
//!
//! This root only mounts that source as a module and re-exports all of it, so
//! `blaze_inproc_rt::X` names what the library's root defines. Every item of that root is
//! `pub`, so the glob re-exports the whole crate, and the source's own `crate::` paths resolve
//! through it.
//!
//! Why a root of its own and not `[lib] path = "../../library/blaze_inproc_rt/src/lib.rs"`:
//! the stage 0 blaze (0.20261030, whose `blaze check` loads the workspace through its old
//! front end) compares the root cargo's unit graph names, `..` and all, with the root its load
//! has, normalized, and refuses the target as one "the workspace was not loaded with". A root
//! inside the package's directory is the same path both ways. Modules below a `#[path]` file
//! resolve beside that file, so `mod ask;` there is library/blaze_inproc_rt/src/ask.rs.

#[path = "../../../library/blaze_inproc_rt/src/lib.rs"]
mod source;

pub use source::*;
