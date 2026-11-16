//! A body's names as written: the checker asks `resolve_path` by name hashes
//! (`Name::of(text)`), and the names library resolves written text, so each name the body's
//! lowering interned is read back from its range in the lowering's own text (the item's text
//! or a macro's expansion, [`View::text_at`]). A hash the table does not hold is the
//! [`Unspelled`] refusal, never a guess.

use std::alloc::Global;

use blaze_hash::HashMap;
use blaze_lower::View;
use blaze_lower::form::NameIdx;
use blaze_typeck::Name;

use super::types::Why;

/// The path heads blaze-lower writes as synthetic segments (blaze-lower's `push_segments`).
const FIXED: [&str; 6] = ["self", "super", "crate", "Self", "$crate", "{{root}}"];

/// A name hash the body's lowering holds no text for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unspelled(pub Name);

/// Each name of one body by its hash.
pub struct Spelling<'b> {
    by_name: HashMap<Name, &'b str, Global>,
}

impl<'b> Spelling<'b> {
    /// The names `view`'s lowering interned with a range (a synthetic name has an empty one
    /// and is one of [`FIXED`] or no path segment).
    pub fn of(view: &View<'b>) -> Result<Spelling<'b>, Why> {
        let mut by_name = HashMap::with_capacity_in(view.names.len() + FIXED.len(), Global);
        for f in FIXED {
            by_name.insert(Name::of(f), f);
        }
        for i in 0..view.names.len() {
            let n = NameIdx(i as u32);
            let r = view.name_range(n)?;
            let text = view.text_at(r)?;
            if text.is_empty() {
                continue;
            }
            let text = text.strip_prefix("r#").unwrap_or(text);
            by_name.insert(Name(view.name(n)?), text);
        }
        Ok(Spelling { by_name })
    }

    pub fn get(&self, n: Name) -> Result<&'b str, Unspelled> {
        self.by_name.get(&n).copied().ok_or(Unspelled(n))
    }
}
