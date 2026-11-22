//! Why an archive or an entry was refused (rule 19): the entry's index, the offset of its
//! header, the start of its name, and what was wrong.

use core::fmt;

use blaze_own::AllocFailure;

/// What is wrong with a path or a link's target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathWhy {
    /// No name at all (or only `.` and `/`), for an entry other than the top directory.
    Empty,
    /// Starts at `/`: outside any destination.
    Absolute,
    /// A `..` component.
    Parent,
    /// A NUL byte (only a pax or GNU long name can hold one).
    Nul,
    /// A `\`, which some readers take as a separator.
    Backslash,
    /// Not UTF-8: cargo's names are, and a path is handed out as text.
    NotUtf8,
    /// Longer than the caller allows.
    TooLong { len: u32, most: u32 },
    /// A path the writer was given that is not already in normal form (`a//b`, `./a`, `a/`).
    NotNormal,
}

/// A header field that holds a number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Mode,
    Size,
    Mtime,
    Checksum,
    /// The size of a pax record, or its `size` value.
    Pax,
}

/// What was wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// The header's checksum is not the sum of its bytes.
    Checksum { stored: u64, computed: u64 },
    /// A number field that is no octal or base-256 number, or overflows.
    Number(Field),
    /// One zero block then a block that is not zero: no end, and no header.
    EndBlock,
    /// The input ended inside the archive.
    Truncated,
    /// A pax header or GNU long name bigger than the caller allows.
    MetaOver { size: u64, most: u32 },
    /// A pax record that is no `<len> <key>=<value>\n`.
    PaxRecord,
    /// An entry of a type tar has but this reader does not take (sparse, volume, multi-volume...).
    Kind(u8),
    /// A character or block device or a FIFO: never written to a destination.
    Device(u8),
    /// A link, which the caller's [`crate::Links::Refuse`] refuses.
    LinksRefused,
    /// The entry's path is refused.
    Path(PathWhy),
    /// A link's target is refused.
    Target(PathWhy),
    /// A symbolic link whose target, read from the link's directory, leaves the destination.
    TargetEscapes,
    /// A path that runs through (or is) an earlier symbolic link: writing it would follow
    /// the link.
    ThroughLink,
    /// A symbolic link at a directory an earlier link's target runs through: it would move
    /// where that target points.
    UnderTarget,
    /// A file bigger than the caller allows.
    EntryOver { size: u64, most: u64 },
    /// The files together are bigger than the caller allows.
    TotalOver { most: u64 },
    /// More entries than the caller allows.
    EntriesOver { most: u32 },
    /// A buffer could not be had.
    Alloc(AllocFailure),
}

/// The start of an entry's name, kept in the error (at most 96 bytes, so the error is a value).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Name {
    bytes: [u8; 96],
    len: u8,
    cut: bool,
}

impl Name {
    pub const NONE: Name = Name { bytes: [0; 96], len: 0, cut: false };

    pub(crate) fn of(raw: &[u8]) -> Name {
        let len = raw.len().min(96);
        let mut bytes = [0; 96];
        bytes[..len].copy_from_slice(&raw[..len]);
        Name { bytes, len: len as u8, cut: raw.len() > 96 }
    }

    /// The name's bytes as kept.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("`")?;
        for chunk in self.bytes().utf8_chunks() {
            for c in chunk.valid().chars() {
                write!(f, "{}", c.escape_debug())?;
            }
            for b in chunk.invalid() {
                write!(f, "\\x{b:02x}")?;
            }
        }
        f.write_str(if self.cut { "...`" } else { "`" })
    }
}

/// A refused archive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Error {
    /// The entry's index (0 for the first), counting every header, metadata ones included.
    pub entry: u32,
    /// The offset of the entry's header in the archive.
    pub at: u64,
    /// The entry's name, as far as it was read; empty for a fault before it.
    pub name: Name,
    pub why: Why,
}

impl fmt::Display for PathWhy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathWhy::Empty => f.write_str("is empty"),
            PathWhy::Absolute => f.write_str("is absolute"),
            PathWhy::Parent => f.write_str("has a `..` component"),
            PathWhy::Nul => f.write_str("holds a NUL byte"),
            PathWhy::Backslash => f.write_str("holds a backslash"),
            PathWhy::NotUtf8 => f.write_str("is not UTF-8"),
            PathWhy::TooLong { len, most } => write!(f, "is {len} bytes long, past the most allowed, {most}"),
            PathWhy::NotNormal => f.write_str("is not in normal form (no empty or `.` components, no trailing `/`)"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blaze_tar: entry {} at byte {}", self.entry, self.at)?;
        if !self.name.bytes().is_empty() {
            write!(f, " ({})", self.name)?;
        }
        write!(f, ": {}", self.why)
    }
}

/// What was wrong, without where: an [`Error`]'s message after its entry, offset and name, and
/// the words a test's oracle quotes for a refusal it holds only the `Why` of.
impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Why::Checksum { stored, computed } => {
                write!(f, "the header's checksum is {stored}, its bytes sum to {computed}")
            }
            Why::Number(field) => write!(f, "the {field:?} field is no number"),
            Why::EndBlock => f.write_str("a zero block followed by one that is not zero"),
            Why::Truncated => f.write_str("the input ended inside the archive"),
            Why::MetaOver { size, most } => {
                write!(f, "a pax header or long name of {size} bytes, past the most allowed, {most}")
            }
            Why::PaxRecord => f.write_str("a pax record that is no `<len> <key>=<value>\\n`"),
            Why::Kind(kind) => write!(f, "an entry of type {:?}, which is not read", char::from(kind)),
            Why::Device(kind) => write!(f, "a device or FIFO entry (type {:?})", char::from(kind)),
            Why::LinksRefused => f.write_str("a link, and this archive may hold none"),
            Why::Path(why) => write!(f, "its path {why}"),
            Why::Target(why) => write!(f, "its link target {why}"),
            Why::TargetEscapes => f.write_str("a symbolic link whose target leaves the destination"),
            Why::ThroughLink => f.write_str("its path runs through an earlier symbolic link"),
            Why::UnderTarget => f.write_str("a symbolic link where an earlier link's target runs"),
            Why::EntryOver { size, most } => write!(f, "a file of {size} bytes, past the most allowed, {most}"),
            Why::TotalOver { most } => write!(f, "the files pass the most bytes allowed in all, {most}"),
            Why::EntriesOver { most } => write!(f, "more than the most entries allowed, {most}"),
            Why::Alloc(failure) => write!(f, "{failure}"),
        }
    }
}

impl core::error::Error for Error {}
