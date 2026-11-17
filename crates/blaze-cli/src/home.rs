//! The home's forms (AGENTS.md scope: "An old home after an upgrade is detected, reported and
//! rebuilt; nothing is migrated"). Everything under `.blaze/v2` is derived data. A record is
//! found by a key that holds every form's magic ([`tool`]), so a record of another form is never
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

/// The tool every key holds: every producer's id (decision D105: the table the entry makes once,
/// today each row from the running binary's linker id) and every form it writes, so other code
/// or a form's bump is a new key for every record (rule 21) and an old home's records are
/// misses. Nothing in it is typed in by hand.
pub fn tool(producers: &Producers) -> Tool {
    with_forms(ContentId::of("blaze-cli tool 4", &producers.digest().0))
}

/// Each kind's tool, by `Kind::index` (decision D105): the row of the producer that makes the
/// kind ([`row_of`]) and every form, so an edit to one producer moves only the keys of its kinds
/// and a release that leaves a producer's crates alone keeps its records. The certificate and the
/// pointer keep [`tool`], which every producer moves.
pub fn tools(producers: &Producers) -> [Tool; blaze_drive::key::KINDS] {
    Kind::ALL.map(|kind| with_forms(ContentId::of("blaze-cli kind tool 1", &producers.row(row_of(kind)).0)))
}

/// The producer that makes each kind of record: one exhaustive match, so a new kind is placed
/// here or does not compile.
pub fn row_of(kind: Kind) -> Row {
    match kind {
        Kind::Expansion => Row::Expander,
        Kind::BodyCheck => Row::BodyChecker,
        Kind::CrateScope | Kind::ModuleScope | Kind::Group => Row::Names,
        Kind::Code => Row::CodeBuilder,
        // No row of their own yet: the binary's id, so any rebuilt blaze makes them again.
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
        | Kind::Script => Row::Whole,
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
}
