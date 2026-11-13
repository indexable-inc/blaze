//! The kernel's world over the leaf table's rows, for the front end's steps
//! (`blaze_front_items::steps`): `FILE` and `EXISTS` reads of a file by its path relative to
//! the checkout, the stable name the daemon's carry gives a file too
//! (crates/blaze-cargo/src/serve/front_carry.rs, `Tree`), with the same hashes, so an entry
//! either driver keeps is the other's hit.
//!
//! # The pin, and why it is sound
//!
//! The pin ([`pin_of`]) is a hash over every file-like row (a [`Kind::File`] or
//! [`Kind::Link`]), in path order: its path, its kind, and then its content id where it is
//! hashed, else its stamp. Nothing else is in it: a directory, a hidden directory and its tag
//! answer every read as no file does (`Gone`), so which of them there are cannot change an
//! answer; a path with no row answers `Gone` too, and it is in the pin by its absence.
//!
//! The kernel's claim of a pin ([`blaze_kernel::World::pin`]): while two pins are equal,
//! every read hashes the same at both. A `FILE` read of a hashed row answers its id, which
//! the pin holds; of a missing path or a non-file `Gone`, which the path set (in the pin)
//! fixes; of a file not hashed, nothing: it is [`WorldError::not_now`], so no entry was ever
//! kept on it (a run that asks a read not valued fails, and a lookup passes over a read-list
//! that names it). An `EXISTS` read answers from the kind alone, which the pin holds. So
//! equal pins answer every read alike, and the stamp an unhashed row puts in the pin stands
//! for bytes nobody hashed: it moves the pin where those bytes may have moved, and a row
//! hashed later moves it once more, which only makes two equal worlds' pins differ (a carry
//! with nothing to repair), never two different worlds' pins equal.
//!
//! A command hashes every file its steps will read before it makes the world (`run`): the
//! world is pinned for the whole command, as the kernel asks of a session's world.

#![blaze::sans_io]

use blaze_front_items::steps::{Source, reads};
use blaze_ids::{ContentId, Hasher};
use blaze_kernel::{Pin, Read, Seen, World, WorldError};

use crate::table::{Kind, Row};

const PIN_DOMAIN: &str = "blaze solo pin 1";

/// The domain of an `EXISTS` read's hash: the daemon's
/// (`EXISTS_DOMAIN`, crates/blaze-cargo/src/serve/front_carry.rs:60, private there), so the two
/// answer a read alike. To move into `blaze_front_items::steps::reads` beside the kind it
/// answers.
pub const EXISTS_DOMAIN: &str = "blaze v2 front carry exists 1";

/// What an `EXISTS` read of a file that is there hashes to.
pub fn there() -> ContentId {
    ContentId::of(EXISTS_DOMAIN, b"there")
}

/// The pin of `rows` (sorted by path): see the module's doc for what it covers.
pub fn pin_of(rows: &[Row]) -> Pin {
    let mut hasher = Hasher::new(PIN_DOMAIN);
    // alloc: one stamp's bytes, once a pin
    let mut stamp = Vec::with_capacity(blaze_core::Stamp::BYTES);
    for row in rows.iter().filter(|row| row.kind.is_file_like()) {
        hasher.part(row.path.as_bytes()).part(&[row.kind as u8]);
        match &row.id {
            Some(id) => {
                hasher.part(&[1]).id(id);
            }
            None => {
                stamp.clear();
                row.stamp.encode(&mut stamp);
                hasher.part(&[0]).part(&stamp);
            }
        }
    }
    Pin(hasher.finish())
}

/// One file-like row as the world answers it.
struct Entry {
    path: String,
    kind: Kind,
    id: Option<ContentId>,
    /// The file's text where a step will read it: loaded as it was hashed.
    text: Option<Text>,
}

enum Text {
    Utf8(String),
    /// Bytes that stop being UTF-8 at `at`.
    Not { bytes: Vec<u8>, at: usize },
}

/// The world of one command: the rows as the sweep and the hashing left them, pinned.
pub struct SoloWorld {
    entries: Vec<Entry>,
    pin: Pin,
}

impl SoloWorld {
    /// The world of `rows` (sorted), with `texts` the bytes of the files hashed or read this
    /// command, by path, sorted; taken.
    pub fn new(rows: &[Row], texts: Vec<(String, Vec<u8>)>) -> SoloWorld {
        let mut texts = texts.into_iter().peekable();
        // alloc: the world's entries, once a command
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows.iter().filter(|row| row.kind.is_file_like()) {
            while texts.peek().is_some_and(|(path, _)| *path < row.path) {
                texts.next();
            }
            let text = match texts.peek() {
                Some((path, _)) if *path == row.path => {
                    let (_, bytes) = texts.next().expect("peeked");
                    Some(match String::from_utf8(bytes) {
                        Ok(text) => Text::Utf8(text),
                        Err(e) => {
                            let at = e.utf8_error().valid_up_to();
                            Text::Not { bytes: e.into_bytes(), at }
                        }
                    })
                }
                _ => None,
            };
            // clone: one path a file-like row, the world's own
            entries.push(Entry { path: row.path.clone(), kind: row.kind, id: row.id, text });
        }
        SoloWorld { entries, pin: pin_of(rows) }
    }

    /// The texts it holds, by path, given back: for a world made again over more of them.
    pub fn into_texts(self) -> Vec<(String, Vec<u8>)> {
        self.entries
            .into_iter()
            .filter_map(|entry| match entry.text? {
                Text::Utf8(text) => Some((entry.path, text.into_bytes())),
                Text::Not { bytes, .. } => Some((entry.path, bytes)),
            })
            .collect()
    }

    fn find(&self, path: &str) -> Option<&Entry> {
        self.entries.binary_search_by(|it| it.path.as_str().cmp(path)).ok().map(|at| &self.entries[at])
    }
}

impl World for SoloWorld {
    fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError> {
        let name = || core::str::from_utf8(read.key).map_err(|_| WorldError::new(read, "a path that is not UTF-8"));
        match read.kind {
            reads::FILE => match self.find(name()?) {
                None => Ok(Seen::Gone),
                Some(Entry { kind: Kind::Link, .. }) => {
                    Err(WorldError::new(read, "a symbolic link, which blaze-solo does not follow"))
                }
                Some(Entry { id: Some(id), .. }) => Ok(Seen::Is(*id)),
                Some(Entry { id: None, .. }) => {
                    Err(WorldError::not_now(read, "a file no step read before this command, not hashed"))
                }
            },
            reads::EXISTS => Ok(match self.find(name()?) {
                Some(_) => Seen::Is(there()),
                None => Seen::Gone,
            }),
            reads::CFG => Err(WorldError::new(
                read,
                "a crate's cfg, which blaze-solo cannot answer: it has no crate graph (no build plan)",
            )),
            _ => Err(WorldError::new(read, "a kind of read blaze-solo's world does not answer")),
        }
    }

    fn pin(&self) -> Pin {
        self.pin
    }
}

impl Source for SoloWorld {
    fn file(&self, name: &str) -> Result<Option<&str>, WorldError> {
        let read = Read { kind: reads::FILE, key: name.as_bytes() };
        match self.find(name) {
            None => Ok(None),
            Some(Entry { kind: Kind::Link, .. }) => {
                Err(WorldError::new(read, "a symbolic link, which blaze-solo does not follow"))
            }
            Some(Entry { text: Some(Text::Utf8(text)), .. }) => Ok(Some(text)),
            Some(Entry { text: Some(Text::Not { at, .. }), .. }) => {
                // alloc: an error
                Err(WorldError::new(read, format!("a source file that is not UTF-8 from byte {at}")))
            }
            Some(Entry { text: None, .. }) => Err(WorldError::new(
                read,
                "a file whose text this command did not load: it hashes every file a step reads before it asks",
            )),
        }
    }

    fn cfg(&self, krate: &str, predicate: &str) -> Result<bool, WorldError> {
        // alloc: an error, its read's key
        let key = format!("{krate}\0{predicate}");
        Err(WorldError::new(
            Read { kind: reads::CFG, key: key.as_bytes() },
            "a crate's cfg, which blaze-solo cannot answer: it has no crate graph (no build plan)",
        ))
    }
}
