//! [`RelPath`]: a path beneath one root, in its one canonical spelling (lib.rs, "Paths are
//! checked UTF-8"), and [`normalize_into`], which makes one from a spelling a run used.

#![blaze::sans_io]

use blaze_own::AVec;

/// Why bytes are not a [`RelPath`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathError {
    /// Not UTF-8 (lib.rs: one spelling on every host).
    NotUtf8,
    /// Starts with `/`: a relative path names nothing above its root.
    Absolute,
    /// Ends with `/`, or has two `/` in a row: not the one spelling.
    EmptySegment,
    /// A `.` or `..` segment: not the one spelling (normalize first).
    DotSegment,
    /// A NUL byte, which no file name holds.
    Nul,
    /// A `..` that leaves the root.
    Escapes,
    /// Longer than the read list's [`crate::Limits::most_path`].
    TooLong { most: u32 },
}

/// A path relative to one root, in canonical form: it names the root itself (empty) or a file
/// or directory beneath it, and nothing else. Its only constructors check that.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RelPath<'p>(&'p str);

impl<'p> RelPath<'p> {
    /// The root itself.
    pub const ROOT: RelPath<'static> = RelPath("");

    /// `text`, when it is already in canonical form; `most` is the cap on its bytes.
    pub fn new(text: &'p str, most: u32) -> Result<RelPath<'p>, PathError> {
        check(text.as_bytes(), most)?;
        Ok(RelPath(text))
    }

    /// `bytes` when they are UTF-8 and canonical.
    pub fn from_bytes(bytes: &'p [u8], most: u32) -> Result<RelPath<'p>, PathError> {
        let text = core::str::from_utf8(bytes).map_err(|_| PathError::NotUtf8)?;
        RelPath::new(text, most)
    }

    pub fn as_str(self) -> &'p str {
        self.0
    }

    pub fn is_root(self) -> bool {
        self.0.is_empty()
    }

    /// Its segments, outermost first (none for the root).
    pub fn segments(self) -> impl Iterator<Item = &'p str> {
        self.0.split('/').filter(|segment| !segment.is_empty())
    }
}

/// Checks the canonical form (the module's rules).
fn check(bytes: &[u8], most: u32) -> Result<(), PathError> {
    if u32::try_from(bytes.len()).map_or(true, |len| len > most) {
        return Err(PathError::TooLong { most });
    }
    if bytes.is_empty() {
        return Ok(());
    }
    if bytes[0] == b'/' {
        return Err(PathError::Absolute);
    }
    for segment in bytes.split(|b| *b == b'/') {
        match segment {
            b"" => return Err(PathError::EmptySegment),
            b"." | b".." => return Err(PathError::DotSegment),
            _ if segment.contains(&0) => return Err(PathError::Nul),
            _ => {}
        }
    }
    Ok(())
}

/// Appends to `out` (cleared first) the canonical form of `raw`, read as a path relative to the
/// root: empty and `.` segments dropped, each `..` taking back the segment before it, a `..`
/// with none before it refused ([`PathError::Escapes`]), a leading `/` refused (a mount's
/// spelling is stripped by [`crate::Mounts::resolve`] first). Lexical only (lib.rs).
pub fn normalize_into(raw: &[u8], most: u32, out: &mut AVec<'_, u8>) -> Result<(), PathError> {
    out.clear();
    if raw.first() == Some(&b'/') {
        return Err(PathError::Absolute);
    }
    if core::str::from_utf8(raw).is_err() {
        return Err(PathError::NotUtf8);
    }
    push_segments(raw, out)?;
    check(out.as_slice(), most)
}

/// Appends `raw`'s segments to the canonical path in `out`, resolving `.` and `..` lexically.
pub(crate) fn push_segments(raw: &[u8], out: &mut AVec<'_, u8>) -> Result<(), PathError> {
    for segment in raw.split(|b| *b == b'/') {
        match segment {
            b"" | b"." => {}
            b".." => {
                if out.is_empty() {
                    return Err(PathError::Escapes);
                }
                let keep = out.iter().rposition(|b| *b == b'/').unwrap_or(0);
                out.truncate(keep);
            }
            _ if segment.contains(&0) => return Err(PathError::Nul),
            _ => {
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(segment);
            }
        }
    }
    Ok(())
}
