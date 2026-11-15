//! A `#[deprecated]` function kept as a thin forwarder to its replacement while callers move
//! (the rule-30 migration step "merge duplicates and ban the removed name").

pub mod place {
    /// The one home.
    pub fn cargo_home(root: &str) -> String {
        format!("{root}/.cargo")
    }

    /// Replaced by `cargo_home`.
    #[deprecated = "use place::cargo_home"]
    pub fn home_of(root: &str) -> String {
        cargo_home(root)
    }

    pub fn join(a: &str, b: &str) -> String {
        format!("{a}/{b}")
    }

    #[deprecated = "use place::join"]
    pub fn joined(a: &str, b: &str) -> String {
        join(a, b)
    }

    // Not ported: it forwards its parameters swapped.
    #[deprecated = "use place::join"]
    pub fn under(a: &str, b: &str) -> String {
        join(b, a)
    }

    pub fn width(n: u32) -> u32 {
        n * 2
    }

    // Not ported: deprecated, but its body does more than forward.
    #[deprecated = "use width"]
    pub fn doubled(n: u32) -> u32 {
        width(n) + 0
    }
}

pub mod config {
    #[allow(deprecated)]
    pub fn configured(root: &str) -> String {
        // Ported: a call through the module path.
        crate::place::cargo_home(root)
    }

    #[allow(deprecated)]
    pub fn paths(root: &str) -> (String, String) {
        // Ported: `joined`; `under` stays.
        (crate::place::join(root, "bin"), crate::place::under(root, "bin"))
    }

    #[allow(deprecated)]
    pub fn both(root: &str) -> (String, u32) {
        use crate::place::doubled;
        use crate::place::cargo_home;
        // Ported: a call through a `use`; `doubled` stays.
        (cargo_home(root), doubled(3))
    }
}

// Not ported: a function of the same name that is not deprecated.
pub fn home_of(root: &str) -> String {
    root.to_owned()
}

pub fn local(root: &str) -> String {
    home_of(root)
}
