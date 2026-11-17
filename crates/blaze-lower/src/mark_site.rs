//! Where a path in expanded text is resolved, by the mark of its first segment: the ONE
//! function for it (drive.md, "A token keeps its mark", ruled 21:27; rule 30). Its three uses
//! are `$crate` (the crate the mark's definition is in), a name a `macro` item's body writes
//! (resolved where the macro is defined), and a body macro call's own name; every resolver of
//! a body's paths (blaze-lower `scoped::Scoped`, blaze-ladder front2 `ScopedBy` and
//! `BodyWorld::path`, the scoped tests' `Ask`) asks it and nothing else.
//!
//! The rules (the Reference, "Macros By Example", Hygiene: "By default, all identifiers
//! referred to in a macro expansion are looked up as if they appeared at the invocation
//! site", with `$crate` the exception, "the crate where the macro was defined"; a `macro`
//! item is def-site, rustc's opaque transparency):
//! - `$crate` names the crate of its token's mark's definition; a `$crate` with no mark is
//!   refused by name ([`SiteRefused::UnmarkedDollarCrate`]): it is in no expansion.
//! - a path whose first segment's mark is opaque (a `macro` item's, or a token a macro made
//!   def-site) is resolved in the definition's module;
//! - every other path (no mark: the call's own text; a `macro_rules!`'s semi-opaque mark; a
//!   transparent one) is resolved at the CALL, the module the body is in. An ITEM name a
//!   `macro_rules!` writes resolves at the call (only locals, labels and `$crate` are
//!   def-site there, and locals and labels are the lowering's, never a module's).
//!
//! Sans-IO, no allocation.

use blaze_expand::Transparency;
use blaze_surface::{CrateItemKey, ItemKey};

/// A token's mark as an expansion's text keeps it: its one home is blaze-expand's origin
/// column.
pub use blaze_expand::origin_column::TextMark;

/// Where a path is read: the module its first segment is looked up in, and the crate its
/// `$crate` names when its first segment is `$crate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathSite {
    pub module: ItemKey,
    /// `Some` exactly when the first segment is `$crate`: the mark's definition's crate.
    pub dollar_crate: Option<CrateItemKey>,
}

/// A path whose mark gives it no site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteRefused {
    /// `$crate` on a token no expansion wrote: the body's own text, or a call's input.
    UnmarkedDollarCrate,
    /// A relative path under an opaque mark whose definition records no module (a mark no
    /// `macro` item made): where its names are is not known, and the call's module is not
    /// taken in its place.
    OpaqueWithoutModule { krate: CrateItemKey },
}

impl core::fmt::Display for SiteRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SiteRefused::UnmarkedDollarCrate => f.write_str("`$crate` outside an expansion (no mark names its crate)"),
            SiteRefused::OpaqueWithoutModule { krate } => {
                write!(f, "a relative path under a def-site mark of crate {krate:?} whose definition names no module (drive.md, \"A token keeps its mark\")")
            }
        }
    }
}

/// Where a path is resolved: `mark` is its FIRST segment's token's mark (`None` for a token
/// of the text as written), `first` that segment's text, `absolute` whether it is written
/// with a leading `::` (or the lowering's `{{root}}`), `call` the module the body is in.
pub fn site_of(mark: Option<TextMark>, first: &str, absolute: bool, call: ItemKey) -> Result<PathSite, SiteRefused> {
    let dollar = !absolute && first == "$crate";
    let dollar_crate = match (dollar, mark) {
        (false, _) => None,
        (true, Some(m)) => Some(m.site.krate),
        (true, None) => return Err(SiteRefused::UnmarkedDollarCrate),
    };
    let module = match mark {
        Some(m) if m.transparency == Transparency::Opaque => match m.site.module {
            Some(module) => module,
            // A global path and `$crate` start at a crate, never at a module's names: the
            // module is read only for the crate it is in, the call's (the edition's extern
            // prelude), which is the body's.
            None if absolute || dollar => call,
            None => return Err(SiteRefused::OpaqueWithoutModule { krate: m.site.krate }),
        },
        _ => call,
    };
    Ok(PathSite { module, dollar_crate })
}

#[cfg(test)]
mod tests {
    use blaze_expand::{DefSite, Edition};
    use blaze_surface::ContentId;

    use super::*;

    fn key(b: u8) -> ItemKey {
        ItemKey(ContentId([b; 32]))
    }

    fn mark(transparency: Transparency, module: Option<ItemKey>) -> TextMark {
        TextMark { site: DefSite { krate: CrateItemKey(ContentId([9; 32])), module }, transparency, edition: Edition::E2021 }
    }

    /// The Reference, "Macros By Example", Hygiene: a `macro_rules!` (semi-opaque) writes an
    /// item name resolved at the call, a `macro` (opaque) one resolved at its definition, and
    /// `$crate` names the definition's crate under either. Expected: `test
    /// mark_site::tests::the_site_by_the_mark ... ok`.
    #[test]
    fn the_site_by_the_mark() {
        let (call, def) = (key(1), key(2));
        let rules = mark(Transparency::SemiOpaque, None);
        let item = mark(Transparency::Opaque, Some(def));
        assert_eq!(site_of(Some(rules), "only_dep", false, call), Ok(PathSite { module: call, dollar_crate: None }));
        assert_eq!(site_of(Some(item), "sel", false, call), Ok(PathSite { module: def, dollar_crate: None }));
        assert_eq!(site_of(None, "f", false, call), Ok(PathSite { module: call, dollar_crate: None }));
        let krate = Some(CrateItemKey(ContentId([9; 32])));
        assert_eq!(site_of(Some(rules), "$crate", false, call), Ok(PathSite { module: call, dollar_crate: krate }));
        assert_eq!(site_of(Some(item), "$crate", false, call), Ok(PathSite { module: def, dollar_crate: krate }));
        assert_eq!(site_of(None, "$crate", false, call), Err(SiteRefused::UnmarkedDollarCrate));
        let made = mark(Transparency::Opaque, None);
        assert_eq!(site_of(Some(made), "f", false, call), Err(SiteRefused::OpaqueWithoutModule { krate: CrateItemKey(ContentId([9; 32])) }));
        assert_eq!(site_of(Some(made), "core", true, call), Ok(PathSite { module: call, dollar_crate: None }));
    }
}
