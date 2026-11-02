//! rustc's late lint passes of calls of std's `drop`, `forget`, `from_utf8` and atomics.
//!
//! - `drop_forget_useless`: `DropForgetUseless` (`dropping_references`, `dropping_copy_types`,
//!   `forgetting_references`, `forgetting_copy_types`, `undropped_manually_drops`).
//! - `invalid_from_utf8`: `InvalidFromUtf8` (`invalid_from_utf8_unchecked`,
//!   `invalid_from_utf8`).
//! - `invalid_atomic_ordering`: `InvalidAtomicOrdering` (`invalid_atomic_ordering`).
//!
//! Follows rustc's `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedLateLintModPass`) (MIT or
//! Apache-2.0).

pub(crate) mod drop_forget_useless;
pub(crate) mod invalid_atomic_ordering;
pub(crate) mod invalid_from_utf8;

use base_db::Db;
use blaze_hash::HashMap;
use hir_def::FunctionId;

use crate::lints::bodies::types::util;

/// Which of the diagnostic items a pass looks for each function called is, read from its
/// attributes once per function.
struct FnItems {
    known: &'static [&'static str],
    seen: HashMap<FunctionId, Option<&'static str>>,
}

impl FnItems {
    fn new(known: &'static [&'static str]) -> FnItems {
        FnItems { known, seen: HashMap::default() }
    }

    /// rustc's `get_diagnostic_name` of `def`, when it is one of those looked for.
    fn of(&mut self, db: &Db, def: FunctionId) -> Option<&'static str> {
        let known = self.known;
        *self.seen.entry(def).or_insert_with(|| {
            let name = util::fn_item(db, def)?;
            known.iter().copied().find(|it| *it == name.as_str())
        })
    }
}
