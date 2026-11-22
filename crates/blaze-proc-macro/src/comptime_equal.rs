//! THE WASM SIDE OF THE EQUALITY HARNESS (D175; comptime-native.md, "The equality harness"):
//! the wasm sandbox's read rows ([`Read`]: `env:NAME`, `file:rel`, `dir:rel`, `stat:rel`) as
//! blaze-roots [`Entry`]s and blaze-comptime [`Fact`]s, so `blaze_comptime::first_difference`
//! compares the two paths' inputs row by row.
//!
//! The two sides digest differently (the wasm rows hold `ContentId::of("blaze proc-macro
//! read", ..)`, the native ones blake3 of the bytes), so a digest is never converted: each
//! subject is answered again from the same package through [`Answers`], in the native
//! convention, and the wasm row's presence must agree with that answer (a row whose subject was
//! found on one side and absent on the other is a typed [`AdaptError`], never a quiet row).
//!
//! NOT YET DECLARED: `lib.rs` gets `pub mod comptime_equal;` and Cargo.toml
//! `blaze-comptime.workspace = true` from the crate's owner (the outside edits of D175 step 2).

use blaze_comptime::{Fact, FactKind};
use blaze_roots::{Digest, Entry, RelPath, RootId, Seen, listing_digest};

use crate::sandbox::Read;

/// The package as both runs saw it: the one view each subject is answered from again.
pub trait Answers {
    /// The root a subject's path is under and the path inside it (`/package/..` is the
    /// package's root; `/workspace/<dir>/..` a seen root).
    fn place<'s>(&self, rel: &'s str) -> Option<(RootId, &'s str)>;
    fn file(&self, root: RootId, rel: &str) -> Option<&[u8]>;
    /// A directory's names as its root admits them, sorted bytewise; `None`: not a directory.
    fn dir(&self, root: RootId, rel: &str) -> Option<&[&str]>;
    /// A variable's value as granted; `None` unset (or not granted: asked-and-absent).
    fn env(&self, name: &str) -> Option<&[u8]>;
}

/// Why a wasm row has no native counterpart.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AdaptError {
    /// Not one of the four subject kinds.
    Subject { subject: String },
    /// The path is under no root of the run.
    Unplaced { subject: String },
    /// The path does not fit blaze-roots' path rules (length, `..`).
    Path { subject: String },
    /// The wasm run found the subject and the view finds nothing, or the other way round.
    Disagrees { subject: String, wasm_found: bool },
    /// A directory's names are not sorted (the view broke its contract).
    Unsorted { subject: String },
}

/// Most bytes of one path (blaze-roots' cap for a read list's path).
const MOST_PATH: u32 = 4096;

/// The wasm run's reads as entries and facts, in the wasm run's order (the order
/// `first_difference` compares; the native host records in call order too).
pub fn adapt<'r>(
    reads: &'r [Read],
    view: &impl Answers,
    entries: &mut Vec<Entry<'r>>,
    facts: &mut Vec<Fact<'r>>,
) -> Result<(), AdaptError> {
    for read in reads {
        let subject = read.subject.as_str();
        let owned = || subject.to_owned();
        let (kind, rest) = subject.split_once(':').ok_or_else(|| AdaptError::Subject { subject: owned() })?;
        let wasm_found = read.digest.is_some();
        if kind == "env" {
            let value = view.env(rest);
            if value.is_some() != wasm_found {
                return Err(AdaptError::Disagrees { subject: owned(), wasm_found });
            }
            facts.push(Fact {
                kind: FactKind::Env,
                name: rest.as_bytes(),
                value: value.map(|bytes| Digest(blaze_blake3::hash(bytes))),
            });
            continue;
        }
        let (root, rel) = view.place(rest).ok_or_else(|| AdaptError::Unplaced { subject: owned() })?;
        let path = RelPath::new(rel, MOST_PATH).map_err(|_| AdaptError::Path { subject: owned() })?;
        let seen = match kind {
            "file" => match (view.file(root, rel), view.dir(root, rel)) {
                (Some(bytes), _) => Seen::File { digest: Digest(blaze_blake3::hash(bytes)), len: bytes.len() as u64 },
                // A directory where a file was asked: seen to be one, as the native host records
                // it (blaze-comptime host.rs, `Found::Dir` for `Want::Read`).
                (None, Some(_)) => Seen::Dir,
                (None, None) => Seen::Absent,
            },
            "dir" => match view.dir(root, rel) {
                Some(names) => listing(names, subject)?,
                None => Seen::Absent,
            },
            "stat" => match (view.file(root, rel), view.dir(root, rel)) {
                (Some(bytes), _) => Seen::File { digest: Digest(blaze_blake3::hash(bytes)), len: bytes.len() as u64 },
                (None, Some(_)) => Seen::Dir,
                (None, None) => Seen::Absent,
            },
            _ => return Err(AdaptError::Subject { subject: owned() }),
        };
        if (seen != Seen::Absent) != wasm_found {
            return Err(AdaptError::Disagrees { subject: owned(), wasm_found });
        }
        entries.push(Entry { root, path, seen });
    }
    Ok(())
}

fn listing(names: &[&str], subject: &str) -> Result<Seen, AdaptError> {
    listing_digest(names.iter().copied())
        .map(|digest| Seen::Listing { digest })
        .map_err(|_| AdaptError::Unsorted { subject: subject.to_owned() })
}
