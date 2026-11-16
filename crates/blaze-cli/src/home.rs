//! The home's forms (AGENTS.md scope: "An old home after an upgrade is detected, reported and
//! rebuilt; nothing is migrated"). Everything under `.blaze/v2` is derived data. A record is
//! found by a key that holds every form's magic ([`tool`]), so a record of another form is never
//! fetched, only made again. A file at a fixed path (`last`, `leaves`) is checked by its magic:
//! another form is reported in one line and rebuilt. Bytes that claim this build's form and are
//! malformed are refused by name (rule 3): only those.

use core::fmt;

use blaze_drive::Tool;
use blaze_ids::ContentId;

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

/// The tool every key holds: the running binary's linker id (bstd's `Host::build_id`), and every
/// form it writes, so other code or a form's bump is a new key for every record (rule 21) and an
/// old home's records are misses.
pub fn tool(build_id: &[u8]) -> Tool {
    let mut id = ContentId::of("blaze-cli tool 3", build_id);
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
