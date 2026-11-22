//! The home's forms (AGENTS.md scope: "An old home after an upgrade is detected, reported and
//! rebuilt; nothing is migrated"). Everything under `.blaze/v2` is derived data. A record is
//! found by a key that holds every form's magic (`tools`), so a record of another form is never
//! fetched, only made again. A file at a fixed path (`last`, `leaves`) is checked by its magic:
//! another form is reported in one line and rebuilt. Bytes that claim this build's form and are
//! malformed are refused by name (rule 3): only those.

use core::fmt;

use blaze_drive::{Kind, Tool};
use blaze_ids::ContentId;
use blaze_ids::producer::{Producers, Row};

/// A file at a fixed path whose bytes are another form than this build's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Older {
    pub file: &'static str,
    pub found: [u8; 4],
    pub writes: &'static [u8; 4],
}

impl fmt::Display for Older {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}` is form ", self.file)?;
        for byte in self.found {
            write!(f, "{}", core::ascii::escape_default(byte))?;
        }
        f.write_str(", not this build's ")?;
        for byte in *self.writes {
            write!(f, "{}", core::ascii::escape_default(byte))?;
        }
        f.write_str(": rebuilding it")
    }
}

/// `bytes` when they are form `magic` (or too short to name a form, which the reader refuses
/// as torn); [`Older`] when their first four bytes name another.
pub fn current<'b>(file: &'static str, bytes: &'b [u8], magic: &'static [u8; 4]) -> Result<&'b [u8], Older> {
    match bytes.first_chunk::<4>() {
        Some(found) if found != magic => Err(Older { file, found: *found, writes: magic }),
        _ => Ok(bytes),
    }
}

/// Each kind's tool, by `Kind::index` (decision D105): the row of the producer that makes the
/// kind ([`row_of`]) and every form. Every row is the binary's id today (producers.rs), so any
/// rebuilt blaze moves every kind's key; the split stays so a producer that moves into a crate
/// whose closure is what it runs moves only its own kinds' keys. The certificate and the
/// pointer keep the producers' digest's tool, which every producer moves. The rule set that runs (the step's and
/// `lints-add`'s, lints.rs `lints_of`; `blaze_lint::RuleSet::id`) joins the `FileItems` tool
/// (whether it lists lint units) and the `Lint` tool (which rules a unit's findings are of):
/// another step or another added rule is another key for those two kinds alone (rule 65).
pub fn tools(producers: &Producers, lints: blaze_lint::RuleSet) -> [Tool; blaze_drive::key::KINDS] {
    let rules = lints.id();
    Kind::ALL.map(|kind| {
        let row = producers.row(row_of(kind)).0;
        match kind {
            Kind::FileItems | Kind::Lint => {
                let mut chained = [0u8; 64];
                chained[..32].copy_from_slice(&row);
                chained[32..].copy_from_slice(&rules);
                with_forms(ContentId::of("blaze-cli kind tool with lints 1", &chained))
            }
            _ => with_forms(ContentId::of("blaze-cli kind tool 1", &row)),
        }
    })
}

/// The producer that makes each kind of record: one exhaustive match, so a new kind is placed
/// here or does not compile.
///
/// What keys each kind, and what that identity covers (rule 21: a key covers the code that made
/// its value; rule 52). Every producing function is in this crate, so its code runs over this
/// crate's whole dependency graph: the binary. Since 2026-11-17 every row is the binary's linker
/// id (producers.rs), so every kind's identity covers its producer. Before, the five source rows
/// were digests of named crates' closures (`[package.metadata.blaze] producer`: blaze-expand and
/// blaze-proc-macro `expander`, blaze-lower `lowering`, blaze-ladder `body-checker` and
/// `code-builder`, blaze-scope and blaze-world `names`, blaze-drive all five) plus this crate's
/// own files alone (`producer-own`), and none covered what its kind's maker ran:
///
/// | kind or key | row | made by (in blaze-cli) | ran outside its row before (examples) |
/// |---|---|---|---|
/// | CrateScope (ModuleScope, Group: no task today, work.rs `builds`) | Names | names.rs, names/scope.rs | blaze-ladder (`native_links::table_digest` is in the record, scope.rs); blaze-world's caller code; the `sandbox` feature (a build without it refuses the expansions one with it makes) |
/// | Expansion (no task today; kept expansions, blaze-expand `kept_key`, inside CrateScope) | Expander | macros.rs, macro_files | blaze-world, blaze-manifest; the `sandbox` feature |
/// | BodyCheck | BodyChecker | body_types.rs, body_types/ | blaze-lint, blaze-world (verdicts to findings) |
/// | Code (no task today); code records (blaze-ladder `ProducerId<CodeBuilder>`) | CodeBuilder | program.rs, run_parent.rs | this crate's features and profile |
/// | Units, UnitGraph, Fetch, FileItems, Surface, Signature, ImplBucket, Test, MacroModule, Owners, Script, Links, Native (`ProducerId<Whole>`), Given, Lint | Whole | work.rs and the drive | nothing: always the binary's id |
///
/// The new engine's tools (engine_store.rs `tools`) are these per kind, so the same holds there.
/// The rows stay apart, so a producer whose function moves into a crate whose closure is exactly
/// what it runs can take a source row again (blaze-producers digests such a closure) and keep
/// its records across a rebuilt binary; until then no row claims less than the binary.
pub fn row_of(kind: Kind) -> Row {
    match kind {
        Kind::Expansion => Row::Expander,
        Kind::BodyCheck => Row::BodyChecker,
        Kind::CrateScope | Kind::ModuleScope | Kind::Group => Row::Names,
        Kind::Code => Row::CodeBuilder,
        // No row of their own: the binary's id under the whole row's name.
        // `Signature` and `ImplBucket` are the checker's, placed when a test shows their records
        // made by the body checker's code alone.
        Kind::Units
        | Kind::UnitGraph
        | Kind::Fetch
        | Kind::FileItems
        | Kind::Surface
        | Kind::Signature
        | Kind::ImplBucket
        | Kind::Test
        | Kind::MacroModule
        // Made by the request machine itself (blaze-drive owners.rs), never a task.
        | Kind::Owners
        // A script run is made by the sandbox runner, in no producer's row yet (D105 leg 2).
        | Kind::Script
        // A link table is copied from its scope's record, and the native image is made by
        // blaze-ladder's native_links.rs: in no producer's row yet.
        | Kind::Links
        | Kind::Native
        // The request machine's own answer from what the command gives: never a task's record.
        | Kind::Given
        // Made by blaze-lint's rules (lints.rs), in no producer's row yet: placed when blaze-lint
        // is a producer of its own, so an edit elsewhere keeps the lint records.
        | Kind::Lint => Row::Whole,
    }
}

/// `id` chained with every form this build writes: a form's bump is a new key (rule 21).
fn with_forms(mut id: ContentId) -> Tool {
    for magic in blaze_drive::FORMATS.iter().map(|(_, magic)| *magic).chain([crate::sweep::LEAVES_MAGIC]) {
        let mut chained = [0u8; 36];
        chained[..32].copy_from_slice(&id.0);
        chained[32..].copy_from_slice(magic);
        id = ContentId::of("blaze-cli tool 2 form", &chained);
    }
    Tool(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_form_is_older_and_this_form_or_a_torn_one_is_read() {
        let older = current(".blaze/v2/leaves", b"BCL1rest", b"BCL2");
        assert_eq!(older, Err(Older { file: ".blaze/v2/leaves", found: *b"BCL1", writes: b"BCL2" }));
        assert_eq!(
            std::format!("{}", older.unwrap_err()),
            "`.blaze/v2/leaves` is form BCL1, not this build's BCL2: rebuilding it"
        );
        assert_eq!(current("f", b"BCL2rest", b"BCL2"), Ok(&b"BCL2rest"[..]));
        assert_eq!(current("f", b"BC", b"BCL2"), Ok(&b"BC"[..]), "too short to name a form: the reader refuses it");
    }

    /// Rule 21 across builds: two builds of blaze (two linker ids, one source tree) never share
    /// a key for any kind, the old engine's tools and the new engine's alike. Before 2026-11-17
    /// the `CrateScope` tool was the Names row, a digest of named crates' sources, which two
    /// builds with other code elsewhere (blaze-ladder's table digest, the `sandbox` feature)
    /// shared: this test failed there for every kind but the `Whole` ones. The control: one
    /// build id gives one table.
    #[test]
    fn another_build_moves_every_kinds_key() {
        let rules = crate::lints::Lints::NONE.rules;
        let one = tools(&crate::producers::table(b"one build"), rules);
        let other = tools(&crate::producers::table(b"another build"), rules);
        for kind in Kind::ALL {
            assert_ne!(one[kind.index()], other[kind.index()], "{kind:?} keeps its tool across builds");
        }
        let (new_one, new_other) = (crate::engine_store::tools(&one), crate::engine_store::tools(&other));
        for kind in Kind::ALL {
            assert_ne!(new_one.0[kind.index()], new_other.0[kind.index()], "{kind:?} keeps its engine tool across builds");
        }
        assert_eq!(tools(&crate::producers::table(b"one build"), rules), one, "the control: one build, one table");
        std::println!("home: another build moves all {} kinds' tools", Kind::ALL.len());
    }
}
