//! The found record: what a lowering needs of a crate to take it from its records without
//! reading its package (docs/v2/plan/design/restore.md, "Restart reads records (d)", C).
//!
//! A lowering makes a crate's *plan key* from the plan and from directory IDs the VFS already
//! holds (`blaze_lower`'s `found`), before any file of the package is read and before the
//! crate is in a database. Under [`found_key`] of it the store has this record: the crate's
//! own surface (when nothing a build script's run gives is under it) and the manifest of its
//! package's files, each by its path under the package and its content ID.
//!
//! The invariant: a record is a function of its key (rule keys-cover-every-input), so two
//! writers of one key, in two homes, two checkouts or two builds of blaze sharing the
//! machine's sealed layer, write the same bytes. It says nothing a database's IDs or a
//! machine's paths are in: its paths are under the package (never absolute, never through
//! `..`), sorted and distinct ([`Found::encode`] of equal values is equal bytes, and
//! [`Found::new`] makes equal values of the same files in any order). And it says nothing the
//! plan key does not cover: not what a build script's run gives its crate (cfg, variables,
//! `OUT_DIR` files), which is known only once the run is applied, and not the key the crate's
//! image is filed under, which covers that and the code of the crate's proc macros' libraries
//! (`super::image_key`), neither of which is in a plan key.
//!
//! Until layout 2 (2026-11-14) the record held that filed key and the families written under
//! it. Two writers whose image keys differed (one plan key, other run outputs or proc-macro
//! libraries) gave two values under one found key; the second, in a fresh home on a machine
//! whose sealed layer held the first, was refused by the store, which lost everything that
//! build left for the store and dropped its front end (ripgrep on the build VM, the refusal
//! "the store holds other bytes under 7f98e0838b31 ... they first differ at byte 4", byte 4
//! being where the filed key began). Nothing read the filed key: every serve keys the crate
//! from its database ([`super::image_key`]) and asks the store by that. So the field went, and
//! the families with it, which said what one writer happened to put under its filed key.
//! The crate's key the serve walked (`Found::base`, which a start gave its image under) went
//! for the same reason before it was ever merged: it covers the runs and the proc-macro
//! libraries too, so it is no function of the found key either.

use blaze_ids::{ContentId, Hasher};

use super::key::absolute;

/// The domain of the key a crate's found record is kept under ([`found_key`]).
pub const FOUND_DOMAIN: &str = "blaze v2 found record 2";

/// The record's layout: its first word. A record of another layout is under another key
/// ([`found_key`] covers it) and, were it read all the same, is refused by name
/// ([`FoundError::Layout`]).
///
/// 2: the layout word (`u32`), one byte saying whether the crate's own surface follows (0 or
/// 1) and, when it does, its length (`u32`) and bytes, the count of files (`u32`), then each
/// file as its path's length (`u32`), the path's bytes and its content ID (32 bytes), by
/// path. Every integer little-endian. (1 held the filed key and the families after the
/// layout word: the module's documentation says why they went.)
pub const LAYOUT: u32 = 2;

/// Where the found record of the crate whose plan key is `plan` is, as front end `front_end`
/// (its code identity, the image key's `build`) wrote it: the plan key (which covers what the
/// plan says of the crate, its package's directories and its dependencies' plan keys), the
/// front end, and the record's layout, so a record another front end wrote, or of another
/// layout, is never asked for.
pub fn found_key(plan: ContentId, front_end: ContentId) -> ContentId {
    let mut hasher = Hasher::new(FOUND_DOMAIN);
    hasher.id(&plan).id(&front_end).update(&LAYOUT.to_le_bytes());
    hasher.finish()
}

/// A crate's found record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Found {
    /// The crate's own surface as the lowering keeps it (`blaze_lower`'s
    /// `Lowered::found_surface`: the crate's own part of what a front end above may read of
    /// it, before its dependencies' are folded in), when it has one; opaque here, read by
    /// `Lowered::carry_own_surface`. The plan key covers all it is made from (the crate's
    /// files and its settings with those of every crate below), so a lowering that finds
    /// the record carries it and parses no file of the crate to make it. None for a crate a
    /// build script's run applies to, or one above such a crate: the run's cfgs, variables
    /// and files are in no plan key, so the writer leaves it out (`Lowered::found_surface`)
    /// as the reader leaves it uncarried (`Lowered::take_found`), one rule of the lowering's.
    own_surface: Option<Vec<u8>>,
    /// Each file of the package's source root that is under the package: its path under the
    /// package's directory, `/`-separated, and its content ID. Sorted by path, each once.
    files: Vec<(String, ContentId)>,
}

/// Why a found record was not made or not read: what is wrong and where.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FoundError {
    /// The record's first word is not [`LAYOUT`].
    Layout { found: u32 },
    /// The record ends inside the part named, at byte `at`.
    Truncated { part: &'static str, at: usize },
    /// The record has bytes after its last file, from byte `at`.
    Trailing { at: usize },
    /// The byte saying whether a surface follows is neither 0 nor 1.
    SurfaceFlag { found: u8 },
    /// A path that is not under the package: absolute, empty, through `..` or `.`, with an
    /// empty component, a `\`, or a NUL.
    NotUnderPackage { path: String },
    /// A path that is not valid UTF-8, at file `index`.
    NotUtf8 { index: usize },
    /// Two files of one path, or (read) a path not after the one before it.
    Order { path: String },
    /// More files, or a longer path, than the layout's `u32` counts.
    TooLarge { what: &'static str, len: usize },
}

impl std::fmt::Display for FoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FoundError::Layout { found } => write!(
                f,
                "its layout is {found} and this blaze reads layout {LAYOUT} (rule: a found record of another layout is refused, never read as this one)"
            ),
            FoundError::Truncated { part, at } => {
                write!(f, "it ends inside its {part}, at byte {at}")
            }
            FoundError::SurfaceFlag { found } => {
                write!(f, "the byte saying whether its surface follows is {found}, neither 0 nor 1")
            }
            FoundError::Trailing { at } => write!(f, "it has bytes after its last file, from byte {at}"),
            FoundError::NotUnderPackage { path } => write!(
                f,
                "its file `{path}` is not a path under the package (rule: a found record holds no absolute path and none through `..`)"
            ),
            FoundError::NotUtf8 { index } => write!(f, "the path of its file {index} is not UTF-8"),
            FoundError::Order { path } => write!(
                f,
                "its file `{path}` is not after the file before it (rule: a found record's files are sorted by path, each once)"
            ),
            FoundError::TooLarge { what, len } => {
                write!(f, "its {what} is {len}, more than the layout counts")
            }
        }
    }
}

impl std::error::Error for FoundError {}

/// Whether `path` is a path under a package as a record holds one.
fn under_package(path: &str) -> bool {
    !path.is_empty()
        && !absolute(path)
        && !path.contains(['\\', '\0'])
        && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

impl Found {
    /// The record of a crate whose own surface is `own_surface` and whose package holds
    /// `files`, in any order: sorted here. Refused for a path not under the package and for
    /// two files of one path.
    pub fn new(own_surface: Option<Vec<u8>>, mut files: Vec<(String, ContentId)>) -> Result<Found, FoundError> {
        files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (at, (path, _)) in files.iter().enumerate() {
            if !under_package(path) {
                // clone: an error's path, once
                return Err(FoundError::NotUnderPackage { path: path.clone() });
            }
            if at > 0 && files[at - 1].0 == *path {
                // clone: an error's path, once
                return Err(FoundError::Order { path: path.clone() });
            }
        }
        Ok(Found { own_surface, files })
    }

    /// The crate's own surface, when the writer had one to keep.
    pub fn own_surface(&self) -> Option<&[u8]> {
        self.own_surface.as_deref()
    }

    /// The package's files, each by its path under the package and its content ID, sorted by
    /// path.
    pub fn files(&self) -> &[(String, ContentId)] {
        &self.files
    }

    /// The record's bytes ([`LAYOUT`]). Equal records are equal bytes.
    pub fn encode(&self) -> Result<Vec<u8>, FoundError> {
        let count = u32::try_from(self.files.len())
            .map_err(|_| FoundError::TooLarge { what: "count of files", len: self.files.len() })?;
        let paths: usize = self.files.iter().map(|(path, _)| path.len()).sum();
        // alloc: the record's bytes, handed to the store's put
        let own = self.own_surface.as_ref().map_or(0, Vec::len);
        let mut out = Vec::with_capacity(4 + 5 + own + 4 + self.files.len() * 36 + paths);
        out.extend_from_slice(&LAYOUT.to_le_bytes());
        match &self.own_surface {
            None => out.push(0),
            Some(own) => {
                let len = u32::try_from(own.len())
                    .map_err(|_| FoundError::TooLarge { what: "own surface's length", len: own.len() })?;
                out.push(1);
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(own);
            }
        }
        out.extend_from_slice(&count.to_le_bytes());
        for (path, id) in &self.files {
            let len = u32::try_from(path.len())
                .map_err(|_| FoundError::TooLarge { what: "path's length", len: path.len() })?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(path.as_bytes());
            out.extend_from_slice(&id.0);
        }
        Ok(out)
    }

    /// The record `bytes` are, whole: every rule [`Found::new`] holds a record to is checked
    /// again, so bytes that are no record of this layout are an error naming what is wrong,
    /// never a record with fewer files.
    pub fn decode(bytes: &[u8]) -> Result<Found, FoundError> {
        let mut at = 0;
        let mut take = |len: usize, part: &'static str| take(bytes, &mut at, len, part);
        let word = |bytes: &[u8]| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let id = |bytes: &[u8]| {
            let mut id = [0; 32];
            id.copy_from_slice(bytes);
            ContentId(id)
        };
        let layout = word(take(4, "layout word")?);
        if layout != LAYOUT {
            return Err(FoundError::Layout { found: layout });
        }
        let own_surface = match take(1, "surface flag")?[0] {
            0 => None,
            1 => {
                let len = word(take(4, "own surface's length")?) as usize;
                Some(take(len, "own surface")?.to_vec()) // alloc: the record's bytes, kept
            }
            found => return Err(FoundError::SurfaceFlag { found }),
        };
        let count = word(take(4, "count of files")?) as usize;
        // alloc: the record's files; a count the bytes cannot hold reserves nothing (each
        // file is at least 37 bytes)
        let mut files: Vec<(String, ContentId)> = Vec::with_capacity(count.min(bytes.len() / 37));
        for index in 0..count {
            let len = word(take(4, "file's path length")?) as usize;
            let path = std::str::from_utf8(take(len, "file's path")?)
                .map_err(|_| FoundError::NotUtf8 { index })?;
            let content = id(take(32, "file's content ID")?);
            if !under_package(path) {
                // alloc: an error's path, once
                return Err(FoundError::NotUnderPackage { path: path.to_owned() });
            }
            if files.last().is_some_and(|(before, _)| before.as_str() >= path) {
                // alloc: an error's path, once
                return Err(FoundError::Order { path: path.to_owned() });
            }
            files.push((path.to_owned(), content)); // alloc: the record's file, kept
        }
        if at != bytes.len() {
            return Err(FoundError::Trailing { at });
        }
        Ok(Found { own_surface, files })
    }
}

/// The next `len` bytes of `bytes` from `at`, which moves past them; an error naming `part`
/// where the record ends first.
fn take<'b>(
    bytes: &'b [u8],
    at: &mut usize,
    len: usize,
    part: &'static str,
) -> Result<&'b [u8], FoundError> {
    let end = at.checked_add(len).filter(|end| *end <= bytes.len());
    let Some(end) = end else { return Err(FoundError::Truncated { part, at: *at }) };
    let taken = &bytes[*at..end];
    *at = end;
    Ok(taken)
}

#[cfg(test)]
mod tests {
    use blaze_sim::{Broke, Config, Tape};

    use super::*;

    const PARTS: &[&str] = &["a", "a-b", "a.rs", "a0", "b", "src", "lib.rs", "z", "é"];

    fn id(n: u64) -> ContentId {
        ContentId::of("blaze v2 found record test", &n.to_le_bytes())
    }

    /// A manifest by draws: up to 40 files of up to four components, no file under another
    /// file's path, each path once.
    fn drawn(tape: &Tape) -> Vec<(String, ContentId)> {
        let count: usize = tape.int_in(0..=40, "files");
        let mut files: Vec<(String, ContentId)> = Vec::new();
        for _ in 0..count {
            let depth: usize = tape.int_in(0..=3, "depth");
            let mut parts: Vec<&str> = Vec::new();
            for _ in 0..=depth {
                let part: usize = tape.index(PARTS.len(), "part");
                parts.push(PARTS[part]);
            }
            let path = parts.join("/");
            if !files.iter().any(|(it, _)| *it == path) {
                let content: u64 = tape.int_in(0..=5, "content");
                files.push((path, id(content)));
            }
        }
        files
    }

    /// The invariant: a record read back is the record written, for every accessor, and the
    /// bytes are a function of the surface and the set of files alone: the same files given
    /// in another order encode to the same bytes (what two databases of the same sources
    /// write).
    #[test]
    fn a_record_reads_back_as_written_and_its_bytes_are_of_its_files_alone() {
        blaze_sim::check(Config::DEFAULT, |tape| reads_back_as_written(tape))
    }

    fn reads_back_as_written(tape: &Tape) {
        let files = drawn(tape);
        let kept: usize = tape.int_in(0..=32, "kept");
        let surface = tape.bool("surface").then(|| id(50).0[..kept].to_vec());
        let found = Found::new(surface.clone(), files.clone()).expect("generated paths are under the package");
        let bytes = found.encode().unwrap();
        let read = Found::decode(&bytes).unwrap();
        assert_eq!(read, found);
        assert_eq!(read.own_surface(), surface.as_deref());
        let mut sorted = files.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(read.files(), sorted.as_slice());
        // Another order, as another database's file IDs would list them.
        let mut other = files;
        other.reverse();
        let turn: usize = tape.int_in(0..=7, "turn");
        if !other.is_empty() {
            let by = turn % other.len();
            other.rotate_left(by);
        }
        assert_eq!(Found::new(surface.clone(), other).unwrap().encode().unwrap(), bytes);
        // And the bytes tell records apart: another surface, or one file's content.
        let other_surface = if surface.is_some() { None } else { Some(id(51).0.to_vec()) };
        assert_ne!(Found::new(other_surface, sorted.clone()).unwrap().encode().unwrap(), bytes);
        if let Some(first) = sorted.first_mut() {
            first.1 = id(98);
            assert_ne!(Found::new(surface, sorted).unwrap().encode().unwrap(), bytes);
        }
    }

    /// How a writer of the test makes a crate's record from what its home has: the plan's
    /// part (the files, and the surface they make), and what only that home knows of the crate
    /// (the key its image is filed under, and its build script's run, when it has one).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Writer {
        /// As `deps::serve` writes it: the surface left out when a run applies to the crate.
        AsWritten,
        /// Planted: layout 1's filed key kept in the record (the ripgrep refusal).
        KeepsFiled,
        /// Planted: a surface kept for a crate a run applies to, made with the run's output.
        KeepsRunSurface,
    }

    /// What one home knows of a crate beyond its plan key.
    struct Home {
        image: ContentId,
        run: Option<ContentId>,
    }

    fn write(writer: Writer, files: &[(String, ContentId)], home: &Home) -> Vec<u8> {
        let mut hasher = Hasher::new("blaze v2 found record test surface");
        for (path, content) in files {
            hasher.update(path.as_bytes()).id(content);
        }
        let of_files = hasher.finish().0.to_vec();
        let surface = match (writer, home.run) {
            (Writer::AsWritten | Writer::KeepsFiled, Some(_)) => None,
            (Writer::AsWritten | Writer::KeepsRunSurface, None) => Some(of_files),
            (Writer::KeepsFiled, None) => Some([home.image.0.as_slice(), of_files.as_slice()].concat()),
            (Writer::KeepsRunSurface, Some(run)) => Some([of_files.as_slice(), run.0.as_slice()].concat()),
        };
        Found::new(surface, files.to_vec()).unwrap().encode().unwrap()
    }

    /// The invariant: two writers of one found key, in two homes sharing the machine's sealed
    /// layer, write the same bytes, whatever their image keys and build-script runs; so a
    /// store that refuses two values under one key (as the sealed layer does) takes both puts.
    /// The writers' plan inputs (the files) are the same, as one plan key says; what only a
    /// home knows (its image key, its run's output) is drawn for each home apart.
    #[test]
    fn two_writers_of_one_found_key_write_the_same_bytes() {
        blaze_sim::control(Config::cases(256), Writer::AsWritten, Writer::KeepsFiled, "two values", two_writers);
        blaze_sim::control(
            Config::cases(256),
            Writer::AsWritten,
            Writer::KeepsRunSurface,
            "two values",
            two_writers,
        );
    }

    fn two_writers(tape: &Tape, writer: Writer) -> Result<(), Broke> {
        use crate::nameres::frozen::entries::{Entries, Memory};
        let files = drawn(tape);
        let runs = tape.bool("a run applies");
        let home = |tape: &Tape| Home {
            image: id(tape.int_in(100..=103, "image key")),
            run: runs.then(|| id(tape.int_in(200..=203, "run output"))),
        };
        let (first, second) = (home(tape), home(tape));
        let key = found_key(id(7), id(70));
        let store = Memory::within_one_build();
        let set = id(0);
        store.put(&set, key, &write(writer, &files, &first)).map_err(Broke::Harness)?;
        store.put(&set, key, &write(writer, &files, &second)).map_err(|why| {
            Broke::Oracle(format!("the second writer of found key {key:?} was refused: {why}"))
        })
    }

    /// Bytes that are a record cut short, or with a byte more, are refused naming the part:
    /// never read as a record of fewer files.
    #[test]
    fn a_record_cut_anywhere_is_refused() {
        blaze_sim::check(Config::DEFAULT, |tape| {
            let surface = tape.bool("surface").then(|| id(50).0.to_vec());
            let found = Found::new(surface, drawn(tape)).unwrap();
            let bytes = found.encode().unwrap();
            let cut: usize = tape.index(bytes.len(), "cut");
            assert!(
                matches!(Found::decode(&bytes[..cut]), Err(FoundError::Truncated { .. })),
                "cut at {cut} of {}: {:?}",
                bytes.len(),
                Found::decode(&bytes[..cut])
            );
            let mut more = bytes.clone();
            more.push(0);
            assert_eq!(Found::decode(&more), Err(FoundError::Trailing { at: bytes.len() }));
        })
    }

    #[test]
    fn a_previous_or_foreign_layout_is_refused_by_name() {
        let mut bytes = Found::new(None, vec![("src/lib.rs".to_owned(), id(2))]).unwrap().encode().unwrap();
        bytes[0] = 0;
        let refused = Found::decode(&bytes).unwrap_err();
        assert_eq!(refused, FoundError::Layout { found: 0 });
        assert!(refused.to_string().contains("its layout is 0 and this blaze reads layout 2"), "{refused}");
        // Layout 1 (its filed key and families after the layout word) is refused by name.
        bytes[0] = 1;
        assert_eq!(Found::decode(&bytes), Err(FoundError::Layout { found: 1 }));
        bytes[0] = 3;
        assert_eq!(Found::decode(&bytes), Err(FoundError::Layout { found: 3 }));
        bytes[0] = 2;
        bytes[4] = 7;
        assert_eq!(Found::decode(&bytes), Err(FoundError::SurfaceFlag { found: 7 }));
        // And a record of another layout is never asked for: its key is another.
        let (plan, front_end) = (id(7), id(70));
        let mut other = Hasher::new(FOUND_DOMAIN);
        other.id(&plan).id(&front_end).update(&1u32.to_le_bytes());
        assert_ne!(found_key(plan, front_end), other.finish());
        assert_ne!(found_key(plan, front_end), found_key(id(8), front_end));
        // Another front end's record is never asked for.
        assert_ne!(found_key(plan, front_end), found_key(plan, id(71)));
    }

    /// No path of a machine gets into a record, written or read: each is refused naming it.
    #[test]
    fn a_path_that_is_not_under_the_package_is_refused_written_and_read() {
        for path in ["/home/u/p/src/lib.rs", "C:\\p\\lib.rs", "C:/p/lib.rs", "../x.rs", "src/../../x.rs", "src//x.rs", "src/", "", "./x.rs", "a\\b.rs", "a\0b"] {
            let refused = Found::new(None, vec![(path.to_owned(), id(2))]);
            assert_eq!(refused, Err(FoundError::NotUnderPackage { path: path.to_owned() }), "{path:?}");
            // The same bytes a writer without the rule would have written.
            let unchecked = Found { own_surface: None, files: vec![(path.to_owned(), id(2))] };
            let read = Found::decode(&unchecked.encode().unwrap());
            assert_eq!(read, Err(FoundError::NotUnderPackage { path: path.to_owned() }), "{path:?} read");
        }
        let twice = vec![("a.rs".to_owned(), id(1)), ("a.rs".to_owned(), id(2))];
        assert_eq!(Found::new(None, twice), Err(FoundError::Order { path: "a.rs".to_owned() }));
        let unsorted = Found { own_surface: None, files: vec![("b.rs".to_owned(), id(1)), ("a.rs".to_owned(), id(2))] };
        assert_eq!(
            Found::decode(&unsorted.encode().unwrap()),
            Err(FoundError::Order { path: "a.rs".to_owned() }),
            "the control: a writer that does not sort is caught by the reader"
        );
    }
}
