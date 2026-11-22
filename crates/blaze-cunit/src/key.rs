//! The unit's key, and each atom's key and symbol derived from it (c.md part 3: "Its key: its
//! text, the headers it read, its flags, the target, the toolchain").
//!
//! The UNIT KEY is a BLAKE3 hash, in its own domain, of every input of the object clang writes
//! for one translation unit, each a length-prefixed part in one fixed order:
//!
//! 1. the language ([`Lang`]): C or C++, which the driver passes to clang as `-x`;
//! 2. the source's path, relative to its package's root: `__FILE__` expands to it, so it is an
//!    input of the bytes, and two equal files at two paths are two units;
//! 3. the source's digest;
//! 4. every header the front end read, as (root, path under that root, digest), sorted by (root,
//!    path) and each once: the read list the driver collects (c.md part 1). The roots are named
//!    ([`Root`]), never a host path (rule 25);
//! 5. the flags, IN ORDER: `-D`/`-U` and `-I` are order-dependent, so they are not sorted;
//! 6. the target ([`blaze_native::Platform`]);
//! 7. the toolchain's digest (the shelf's clang record).
//!
//! A part that cannot be in this order is a typed refusal ([`KeyError`]), never sorted for the
//! caller: the read list is the driver's, and a wrong one is the driver's bug (rule 21: the fix
//! is the key's input, never the check).
//!
//! An ATOM's key is the unit key and the atom's name ([`AtomName`]): its record's key in the
//! store. A symbol a unit defines for itself alone (a `static` function, a string literal) has a
//! [`Symbol`] made the same way ([`local_symbol`]), so it names nothing of another unit; a C
//! name a unit exports is `Symbol::of_name` of the name, the symbol Rust's records already name it
//! by (blaze-emit compose/emit.rs:1773), so one name has one symbol (rule 30).

use core::fmt;

use blaze_code::record::Symbol;
use blaze_ids::Hasher;
use blaze_native::Platform;

use crate::split::AtomName;

/// The domain of a unit key.
pub const UNIT_DOMAIN: &str = "blaze-cunit unit key 1";
/// The domain of an atom's record key.
pub const ATOM_DOMAIN: &str = "blaze-cunit atom key 1";
/// The domain of a unit-local symbol.
pub const LOCAL_DOMAIN: &str = "blaze-cunit local symbol 1";

/// One translation unit's identity: every input of its object, hashed ([`unit_key`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct UnitKey(pub [u8; 32]);

impl fmt::Display for UnitKey {
    /// The first 12 hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0[..6].iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// The language clang is told the source is in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    C,
    Cxx,
}

impl Lang {
    fn byte(self) -> u8 {
        match self {
            Lang::C => 0,
            Lang::Cxx => 1,
        }
    }
}

/// Where a header was found: the package's own tree, its `n`th declared include path, or the
/// shelf (the toolchain's own headers: libc, libc++, compiler-rt's). The order is the sort's.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Root {
    Package,
    Include(u32),
    Shelf,
}

impl Root {
    fn bytes(self) -> [u8; 5] {
        let (tag, n) = match self {
            Root::Package => (0u8, 0u32),
            Root::Include(n) => (1, n),
            Root::Shelf => (2, 0),
        };
        let n = n.to_le_bytes();
        [tag, n[0], n[1], n[2], n[3]]
    }
}

/// One header the front end read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HeaderRead<'a> {
    pub root: Root,
    /// Under `root`, `/`-separated, no `.` or `..` part.
    pub path: &'a str,
    pub digest: [u8; 32],
}

/// Every input of one unit's object.
#[derive(Clone, Copy, Debug)]
pub struct UnitKeyParts<'a> {
    pub lang: Lang,
    /// The source's path under its package's root.
    pub path: &'a str,
    pub source: [u8; 32],
    /// Sorted by (root, path), each once.
    pub headers: &'a [HeaderRead<'a>],
    /// In the order the driver passes them.
    pub flags: &'a [&'a str],
    pub target: Platform,
    pub toolchain: [u8; 32],
}

/// Why a unit has no key: an input not in its one order, naming the header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyError<'a> {
    /// `path` under `root` comes before the header listed ahead of it.
    HeadersUnsorted { root: Root, path: &'a str },
    /// `path` under `root` is listed twice.
    HeaderTwice { root: Root, path: &'a str },
    /// A path (the source's, `of: None`; else the header's) that is empty, absolute, or holds a
    /// `.` or `..` part: no path of a host reaches a key (rule 25).
    Path { of: Option<Root>, path: &'a str },
}

impl fmt::Display for KeyError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "(blaze-cunit key.rs; rule 21: the fix is the read list, never the check)";
        match *self {
            KeyError::HeadersUnsorted { root, path } => write!(f, "a unit's read list has header {path} under {root:?} out of (root, path) order {rule}"),
            KeyError::HeaderTwice { root, path } => write!(f, "a unit's read list has header {path} under {root:?} twice {rule}"),
            KeyError::Path { of: None, path } => write!(f, "a unit's source path {path:?} is not a relative path of plain parts (rule 25: no host path in a key)"),
            KeyError::Path { of: Some(root), path } => write!(f, "a unit's header path {path:?} under {root:?} is not a relative path of plain parts (rule 25: no host path in a key)"),
        }
    }
}

impl core::error::Error for KeyError<'_> {}

/// A relative path of plain parts: not empty, no leading `/`, no empty, `.` or `..` part.
fn plain(path: &str) -> bool {
    !path.is_empty() && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

/// The unit key of `parts` (the module's order).
pub fn unit_key<'a>(parts: &UnitKeyParts<'a>) -> Result<UnitKey, KeyError<'a>> {
    if !plain(parts.path) {
        return Err(KeyError::Path { of: None, path: parts.path });
    }
    let mut previous: Option<&HeaderRead<'a>> = None;
    for header in parts.headers {
        if !plain(header.path) {
            return Err(KeyError::Path { of: Some(header.root), path: header.path });
        }
        if let Some(previous) = previous {
            match (previous.root, previous.path).cmp(&(header.root, header.path)) {
                core::cmp::Ordering::Equal => return Err(KeyError::HeaderTwice { root: header.root, path: header.path }),
                core::cmp::Ordering::Greater => return Err(KeyError::HeadersUnsorted { root: header.root, path: header.path }),
                core::cmp::Ordering::Less => {}
            }
        }
        previous = Some(header);
    }
    let mut h = Hasher::new(UNIT_DOMAIN);
    h.update(&[parts.lang.byte()]);
    h.part(parts.path.as_bytes());
    h.update(&parts.source);
    h.update(&(parts.headers.len() as u64).to_le_bytes());
    for header in parts.headers {
        h.update(&header.root.bytes()).part(header.path.as_bytes()).update(&header.digest);
    }
    h.update(&(parts.flags.len() as u64).to_le_bytes());
    for flag in parts.flags {
        h.part(flag.as_bytes());
    }
    h.update(&[platform_byte(parts.target)]);
    h.update(&parts.toolchain);
    Ok(UnitKey(h.finish().0))
}

/// The target's byte in a key.
fn platform_byte(platform: Platform) -> u8 {
    match platform {
        Platform::MacosArm64 => 0,
        Platform::LinuxArm64 => 1,
    }
}

/// An atom name's bytes in a key: a tag, then the name's own parts.
fn name_parts(h: &mut Hasher, name: &AtomName<'_>) {
    match *name {
        AtomName::Global(name) => {
            h.update(&[0]).part(name);
        }
        AtomName::Local { name, nth } => {
            h.update(&[1]).part(name).update(&nth.to_le_bytes());
        }
        AtomName::Anonymous { section, start } => {
            h.update(&[2]).update(&section.to_le_bytes()).update(&start.to_le_bytes());
        }
    }
}

/// The store key of the record of atom `name` of `unit`.
pub fn atom_key(unit: UnitKey, name: &AtomName<'_>) -> [u8; 32] {
    let mut h = Hasher::new(ATOM_DOMAIN);
    h.update(&unit.0);
    name_parts(&mut h, name);
    h.finish().0
}

/// The symbol of an atom of `unit` that no other unit names (a local or anonymous atom). A
/// global atom's symbol is `Symbol::of_name` of its C name ([`global_symbol`]).
pub fn local_symbol(unit: UnitKey, name: &AtomName<'_>) -> Symbol {
    let mut h = Hasher::new(LOCAL_DOMAIN);
    h.update(&unit.0);
    name_parts(&mut h, name);
    Symbol(h.finish().0)
}

/// The symbol of a C name: the one Rust's records name it by.
pub fn global_symbol(name: &str) -> Symbol {
    Symbol::of_name(name)
}
