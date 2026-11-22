//! The digests a spawn request and answer hold, one home (rule 30): a file's bytes, a served
//! tree, a folder that exists, a lent variable's value. The proc-macro route
//! (blaze-proc-macro/src/spawn_route.rs) wrote the first two as crate-private helpers; the
//! sandbox's answer (blaze-hvbox's job.rs) must give the very same digests for the very same
//! bytes, so both take them from here and the proc-macro copies go (wiring
//! hvbox-digests-1.md says how).
//!
//! No allocation but the scratch the caller passes; no host.

use blaze_ids::ContentId;
use blaze_own::AVec;

/// The domain of a file's digest (`Input::File`, `present`, `Seen::Read`).
pub const FILE_DOMAIN: &str = "blaze v2 spawn file 1";
/// The domain of a tree's digest (`Input::Tree`).
pub const TREE_DOMAIN: &str = "blaze v2 spawn tree 1";
/// The domain of a `Seen::Read` of a folder (opened, not listed: only that it exists).
pub const DIR_DOMAIN: &str = "blaze v2 spawn dir 1";
/// The domain of a `Seen::Read` of a lent variable (`env:<NAME>`).
pub const ENV_DOMAIN: &str = "blaze v2 spawn env 1";

/// A file's digest.
pub fn file_digest(bytes: &[u8]) -> ContentId {
    ContentId::of(FILE_DOMAIN, bytes)
}

/// The digest a `Seen::Read` of a folder carries: one value, since what an open of a folder
/// learns is only that it is one (its names are a `Seen::List`).
pub fn dir_digest() -> ContentId {
    ContentId::of(DIR_DOMAIN, b"")
}

/// A lent variable's digest: its value's bytes.
pub fn env_digest(value: &[u8]) -> ContentId {
    ContentId::of(ENV_DOMAIN, value)
}

/// Why a tree digest was not made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeError {
    /// The files were not strictly sorted by path: the one at `at` is not after the one before.
    Unsorted { at: usize },
    /// A path longer than `u32::MAX` bytes (`at`).
    PathTooLong { at: usize },
    /// The scratch allocator had no room.
    NoRoom,
}

/// A tree's digest, the coarse proof covering every file under it: for each file in path
/// order, its path's length (u32, little-endian), its path (relative to the tree) and its bytes'
/// [`file_digest`]. The bytes are the proc-macro route's, so one tree has one digest whoever
/// computes it. `files` must be strictly sorted by path (refused, never sorted here);
/// `scratch` is cleared and holds the encoding.
pub fn tree_digest<'f, I>(files: I, scratch: &mut AVec<'_, u8>) -> Result<ContentId, TreeError>
where
    I: IntoIterator<Item = (&'f [u8], &'f [u8])>,
{
    scratch.clear();
    let mut prev: Option<&[u8]> = None;
    for (at, (path, bytes)) in files.into_iter().enumerate() {
        if prev.is_some_and(|p| p >= path) {
            return Err(TreeError::Unsorted { at });
        }
        prev = Some(path);
        let len = u32::try_from(path.len()).map_err(|_| TreeError::PathTooLong { at })?;
        scratch.try_extend_from_slice(&len.to_le_bytes()).map_err(|_| TreeError::NoRoom)?;
        scratch.try_extend_from_slice(path).map_err(|_| TreeError::NoRoom)?;
        scratch.try_extend_from_slice(&file_digest(bytes).0).map_err(|_| TreeError::NoRoom)?;
    }
    Ok(ContentId::of(TREE_DOMAIN, scratch.as_slice()))
}

/// `digest` as 64 lowercase hex digits.
pub fn hex(digest: &[u8; 32]) -> [u8; 64] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 64];
    for (i, byte) in digest.iter().enumerate() {
        out[2 * i] = DIGITS[usize::from(byte >> 4)];
        out[2 * i + 1] = DIGITS[usize::from(byte & 15)];
    }
    out
}

/// 64 lowercase hex digits back to the 32 bytes, or `None` for anything else (an upper-case
/// digit too: one digest has one spelling).
pub fn unhex(text: &[u8]) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, pair) in text.chunks_exact(2).enumerate() {
        out[i] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use blaze_own::{AVec, Alloc};

    use super::*;

    /// The proc-macro route's bytes, written out by hand: a refactor that changes the encoding
    /// changes every key, and this test names it.
    #[test]
    fn tree_digest_is_the_proc_macro_routes_encoding() {
        let alloc = Alloc::global();
        let mut scratch = AVec::new_in(alloc);
        let files: [(&[u8], &[u8]); 2] = [(b"a.rs", b"fn a() {}"), (b"src/b.rs", b"")];
        let got = tree_digest(files, &mut scratch).unwrap();
        let mut want = std::vec::Vec::new();
        for (path, bytes) in files {
            want.extend_from_slice(&(path.len() as u32).to_le_bytes());
            want.extend_from_slice(path);
            want.extend_from_slice(&ContentId::of("blaze v2 spawn file 1", bytes).0);
        }
        assert_eq!(got, ContentId::of("blaze v2 spawn tree 1", &want));
    }

    #[test]
    fn an_unsorted_tree_is_refused_and_hex_round_trips() {
        let alloc = Alloc::global();
        let mut scratch = AVec::new_in(alloc);
        let files: [(&[u8], &[u8]); 2] = [(b"b", b""), (b"a", b"")];
        assert_eq!(tree_digest(files, &mut scratch), Err(TreeError::Unsorted { at: 1 }));
        let d = file_digest(b"x").0;
        assert_eq!(unhex(&hex(&d)), Some(d));
        let mut upper = hex(&d);
        upper.make_ascii_uppercase();
        assert!(upper.iter().any(u8::is_ascii_uppercase) == false || unhex(&upper).is_none());
    }
}
