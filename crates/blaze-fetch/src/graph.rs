//! The fetch graph, shaped now and built later: Nix's fixed-output derivations done better.
//!
//! An impure source ([`crate::Source`], fetched by [`crate::Fetcher::get`]) feeds a chain of
//! pure steps ([`Transform`]: unpack, patch, vendor, filter), and only the chain's final output
//! is pinned. Where Nix pins a derivation's output and runs its builder in a sandbox it trusts
//! to be deterministic, a step here is pure by the compiler's proof (`#[blaze::pure]`), so its
//! output is a function of its input and its code, and can be keyed by them:
//!
//! - Each step's output is filed under [`crate::Origin::Step`]: the step's code identity (the
//!   kernel's per-function code ID) and each input's [`crate::Object::id`]. A step run once on
//!   an input is never run on it again.
//! - The final output is checked against the pin by [`crate::ensure`], through the same one
//!   function a download's bytes go through (the store's `check_and_file`, which hashes what it
//!   is given itself), and skips the whole graph when the store already has the final pin.
//! - A mismatch names the step ([`crate::Producer::Step`]) and both digests, and prints the
//!   got-digest to paste as the pin.
//!
//! What is still to build (the extension points): a runner that walks a chain of steps,
//! looking each up by [`crate::Origin::Step`] before it runs it, and keeps the output's tree
//! with [`crate::Objects::keep_step`]; the code identity of a step,
//! from the front end's code-identity hash of `T::run` and everything it reaches; and a way to
//! write a graph down (a manifest or a `blaze fetch` request with steps).

use std::fmt;

use crate::tree::Kind;

/// A pure step of a fetch graph: its output is a function of its input alone.
///
/// The attribute makes every method of every impl `#[blaze::pure]`
/// (crates/hir-def/src/attrs.rs, `PureAttr`): reading a clock, the environment, a file or
/// the network in `run` is refused by `blaze::pure`. That lint is deny by default and can be
/// allowed; making it a hard error for impls of this trait is left (see the handoff).
#[blaze::pure]
pub trait Transform {
    /// What the step takes: an object's bytes, a tree, or the outputs of earlier steps.
    type In;

    fn run(input: Self::In) -> Result<Tree, StepError>;
}

/// What a step makes: files by their `/`-separated paths, as a [`crate::tree`] listing holds
/// them.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Tree {
    pub files: Vec<TreeFile>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TreeFile {
    pub path: String,
    pub kind: Kind,
    pub bytes: Vec<u8>,
}

/// Why a step made nothing: its input is not one it can take, said in the step's words.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StepError {
    /// The step, by its name.
    pub step: String,
    pub why: String,
}

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the step `{}` refused its input: {}", self.step, self.why)
    }
}

impl std::error::Error for StepError {}
