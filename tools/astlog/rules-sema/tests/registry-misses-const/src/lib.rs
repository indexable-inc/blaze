//! The shape of `crates/blaze-profile/src/schema.rs`: one `const` per table, and a registry
//! array of them (`pub const TABLES: [&Table; 31]`) that every writer and reader walks.

pub struct Table {
    pub name: &'static str,
}

pub mod schema {
    use super::Table;

    pub const SPANS: Table = Table { name: "spans" };
    pub const COUNTERS: Table = Table { name: "counters" };
    pub const BUILDS: Table = Table { name: "builds" };
    // Found: in no registry, so it is never written or read.
    pub const PANICS: Table = Table { name: "panics" };
    pub const TILES: Table = Table { name: "tiles" };

    pub const TABLES: [&Table; 3] = [&SPANS, &COUNTERS, &BUILDS];
    // Not found: `TILES` is in this second registry.
    pub const RESULTS: [&Table; 1] = [&TILES];
}

pub mod other {
    use super::Table;

    // Not found: a module with no registry has nothing to miss.
    pub const LOOSE: Table = Table { name: "loose" };
}
