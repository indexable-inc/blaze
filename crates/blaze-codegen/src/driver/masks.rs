//! The forms of the masked IR every build of a daemon folds (#1292,
//! docs/v2/plan/design/compile-time.md), hash-consed: a form seen by an earlier build
//! is not hashed again. A function's masked IR itself is made from its IR in each build that
//! folds it: no key known before preparing holds every input of its data objects' content.

use crate::fold::Forms;

pub struct Masks {
    forms: Forms,
}

impl Default for Masks {
    fn default() -> Masks {
        Masks::new()
    }
}

impl Masks {
    pub fn new() -> Masks {
        Masks { forms: Forms::new() }
    }

    /// The masks every build of this process keeps.
    pub fn shared() -> &'static Masks {
        #[expect(
            clippy::disallowed_types,
            reason = "the daemon's one set of folded forms, kept across its builds so a form is hashed once"
        )]
        static SHARED: std::sync::LazyLock<Masks> = std::sync::LazyLock::new(Masks::new);
        &SHARED
    }

    /// How many distinct forms are kept.
    pub fn forms(&self) -> usize {
        self.forms.len()
    }

    /// The forms, for [`crate::fold::Masking::of`].
    pub(super) fn table(&self) -> &Forms {
        &self.forms
    }
}
