//! A name that is missing because the call that would have written it FAILED reports that call,
//! never "not found" (rules 19 and 54). A macro call whose expansion holds an error (no
//! `macro_rules!` arm matched, `compile_error!`, a proc macro's error) writes nothing and is
//! stored refused (resolve.rs `expand_decls`, then [`crate::refused`]): its module's scope holds
//! [`blaze_surface::ScopeError::Refused`] and a path that misses there answers
//! [`Why::Refused`] with the call's key. This joins that key to the call's typed failure, the
//! crate's [`ExpandNote::Failed`] note: the macro's path as written, whether the call is the
//! implicit `::std::main` of D104 (a blaze-edition binary's root `fn main` is its input), and
//! the matcher's own reason ([`blaze_expand::Failed`]).
//!
//! One home: the refusal is [`Why::Refused`]; this only reads the note the refusal points at.

use core::fmt;

use blaze_expand::Failed;
use blaze_id::ItemKey;

use crate::names::{PathError, Why};
use crate::resolve::{ExpandNote, Names, NamesNote};
use crate::task::Loc;

/// A failed macro call a lookup missed a name behind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FailedCall<'n> {
    /// The name the lookup asked for (`main`).
    pub name: &'n str,
    /// The call's key ([`Why::Refused`]'s `call`) and the module it is written in.
    pub call: ItemKey,
    pub module: ItemKey,
    /// The macro's path as written, or task/lower.rs `IMPLICIT_MAIN` (`::std::main`) for the
    /// implicit call.
    pub path: &'n str,
    /// The call no attribute in the text names: the root `fn main`'s `::std::main` (D104).
    pub implicit: bool,
    /// Where the call stands in its file, for a printer (rule 71: printed only).
    pub at: Option<Loc>,
    /// Why it failed: the expansion's own error.
    pub failed: Failed<'n>,
}

impl<'n> FailedCall<'n> {
    /// The failed call behind `error`, a lookup of `name` that missed: `None` when the miss is
    /// not behind a refused call, or the refusal is not a failed expansion (an `env!` this
    /// build cannot answer has its own words in [`Why::Refused`]'s `why`).
    pub fn of(names: &'n Names<'n>, error: &PathError, name: &'n str) -> Option<FailedCall<'n>> {
        let Why::Refused { module, call, .. } = error.why else { return None };
        // Only on an error's path: every crate's notes, in order, the first note of the call.
        (0..names.crates().len()).find_map(|krate| {
            names.notes(krate).iter().find_map(|note| match *note {
                NamesNote::Expand { call: c, at, note: ExpandNote::Failed { path, implicit, failed } } if c == call => {
                    Some(FailedCall { name, call, module, path, implicit, at, failed })
                }
                _ => None,
            })
        })
    }
}

impl fmt::Display for FailedCall<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let FailedCall { name, call, module, path, implicit, failed, .. } = *self;
        if implicit {
            write!(
                f,
                "`{name}` is not written: `fn {name}` is the input of the implicit `#[{path}]` (a blaze-edition binary's root `fn main`, D104; the call's input is the whole item), and that call failed: {failed}"
            )?;
        } else {
            write!(f, "`{name}` is not written: the call of `{path}` in its module failed, and it could have written `{name}`: {failed}")?;
        }
        write!(f, " (macro call {call:x} in module {module:x})")
    }
}
