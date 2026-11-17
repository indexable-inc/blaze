//! The attributes `git status` applies when it hashes a worktree file to compare it with its
//! index entry ("clean", in gitattributes(5)'s words), as far as blaze builds them: `text=auto`
//! on every path, and nothing else that changes bytes. Every other byte-changing form is
//! refused by name ([`Attribute`]), never guessed.
//!
//! gitattributes(5), `text`, "Set to string value "auto"": "Git decides by itself whether the
//! file is text or binary. If it is text and the file was not already in Git with CRLF endings,
//! line endings are converted on checkin". git's convert.c says how (`crlf_to_git` under
//! `CRLF_AUTO`, the same for `CRLF_AUTO_INPUT` and `CRLF_AUTO_CRLF`, so `core.eol` and
//! `core.autocrlf` change nothing on clean once `text=auto` is in force):
//! - the file's statistics ([`text_stats`], convert.c `gather_stats`);
//! - no CR LF pair: nothing to convert, the raw bytes are hashed;
//! - binary ([`TextStats::binary`], convert.c `convert_is_binary`): raw bytes;
//! - the INDEX blob of the same path holds CR LF and is itself text (convert.c
//!   `has_crlf_in_index`, [`blob_has_crlf`]): raw bytes ("not already in Git with CRLF");
//! - else every CR is removed (a text file under `auto` has no lone CR, so each CR is the first
//!   half of a CR LF pair: convert.c strips them without looking at what follows).
//!
//! `status` hashes with neither `HASH_WRITE_OBJECT` nor `HASH_RENORMALIZE`, so `core.safecrlf`
//! never stops it and the index blob is always consulted.

use crate::git::Refused;

/// An attribute form that may change the bytes git hashes for a path and that blaze does not
/// apply: a `.gitattributes` (or `info/attributes`) line naming it makes `status` refuse with
/// [`Refused::AttributeNotBuilt`]. Forms that only remove an attribute's effect (`-filter`,
/// `!filter`, `-ident`, `!ident`, `-eol`, `!eol`, `!crlf`, `-working-tree-encoding`,
/// `!working-tree-encoding`) and attributes that never touch bytes (`diff`, `merge`,
/// `linguist-*`, `export-ignore`, ...) are read past.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Attribute {
    /// `text` set, unset (`-text`), unspecified (`!text`, which hands the path back to
    /// `core.autocrlf`, unread) or given a value other than `auto` (gitattributes(5): "Any other
    /// value causes Git to act as if text has been left unspecified").
    Text,
    /// `text=auto` on a pattern other than `*`, or `* text=auto` in a folder's `.gitattributes`
    /// when the top file does not already put it on every path: which paths it covers then is
    /// pattern matching, which is not built.
    TextAutoNotEverywhere,
    /// `eol` set or given a value (it also sets `text` on its paths).
    Eol,
    /// The old `crlf` attribute in any form but `!crlf` (`crlf` is `text`, `-crlf` is `-text`,
    /// `crlf=input` is `eol=lf`).
    Crlf,
    /// `filter=<driver>`: a clean command blaze does not run.
    Filter,
    /// `ident` set: `$Id$` is collapsed on clean.
    Ident,
    /// `working-tree-encoding` set or given a value: the bytes are re-encoded on clean.
    WorkingTreeEncoding,
    /// The built-in macro `binary` in any form (`binary` is `-diff -merge -text`; `-binary` and
    /// `!binary` set or unspecify `text` on its paths).
    Binary,
    /// An `[attr]` line defining a macro: its uses are not expanded.
    MacroDefinition,
    /// A quoted pattern without its closing quote.
    BadQuoting,
}

/// git's attribute-file whitespace (attr.c `blank`).
fn blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// Where an attributes file sits, which decides what its pattern `*` covers (gitattributes(5):
/// patterns in a folder's `.gitattributes` are relative to that folder, and a deeper file
/// overrides a shallower one for the same attribute).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AttributesFile {
    /// The worktree's top `.gitattributes`, or `$GIT_COMMON_DIR/info/attributes`: `*` is every
    /// path in the repository.
    Top,
    /// A folder's `.gitattributes`: `*` is every path under that folder. `top_sets_auto` says
    /// whether the top files already put `text=auto` on every path. Then a nested
    /// `* text=auto` sets the value already in force and changes nothing.
    Nested { top_sets_auto: bool },
}

/// Whether the attributes file `text` puts `text=auto` on every path it covers, after refusing
/// by name every byte-changing form it holds that blaze does not apply. A `* text=auto` line is
/// applied in a [`AttributesFile::Top`] file, and accepted in a nested one only under the top
/// rule. Lines as attr.c `parse_attr_line` reads them: leading blanks skipped, empty and `#`
/// lines skipped, the pattern (C-quoted when it starts with `"`), then blank-separated
/// attributes, each `name`, `-name`, `!name` or `name=value`. Cost: one pass over the bytes,
/// nothing allocated.
pub(crate) fn sets_text_auto(text: &[u8], file: AttributesFile) -> Result<bool, Refused> {
    let everywhere = match file {
        AttributesFile::Top => true,
        AttributesFile::Nested { top_sets_auto } => top_sets_auto,
    };
    let refuse = |which| Err(Refused::AttributeNotBuilt(which));
    let mut auto = false;
    for line in text.split(|b| *b == b'\n') {
        let start = line.iter().position(|b| !blank(*b)).unwrap_or(line.len());
        let line = &line[start..];
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let (pattern, rest) = if line[0] == b'"' {
            // A C-quoted pattern: a backslash takes the byte after it.
            let mut at = 1;
            loop {
                match line.get(at) {
                    None => return refuse(Attribute::BadQuoting),
                    Some(b'\\') => at += 2,
                    Some(b'"') => break,
                    Some(_) => at += 1,
                }
            }
            (&line[..=at], &line[at + 1..])
        } else {
            let end = line.iter().position(|b| blank(*b)).unwrap_or(line.len());
            if line[..end].starts_with(b"[attr]") {
                return refuse(Attribute::MacroDefinition);
            }
            (&line[..end], &line[end..])
        };
        for token in rest.split(|b| blank(*b)).filter(|token| !token.is_empty()) {
            let (sign, body) = match token[0] {
                b'-' | b'!' => (Some(token[0]), &token[1..]),
                _ => (None, token),
            };
            let (name, value) = match body.iter().position(|b| *b == b'=') {
                Some(eq) => (&body[..eq], Some(&body[eq + 1..])),
                None => (body, None),
            };
            match name {
                b"text" => {
                    if sign.is_some() || value != Some(&b"auto"[..]) {
                        return refuse(Attribute::Text);
                    }
                    if !everywhere || pattern != b"*" {
                        return refuse(Attribute::TextAutoNotEverywhere);
                    }
                    auto = true;
                }
                b"eol" if sign.is_none() => return refuse(Attribute::Eol),
                b"crlf" if sign != Some(b'!') => return refuse(Attribute::Crlf),
                b"filter" if sign.is_none() && value.is_some() => return refuse(Attribute::Filter),
                b"ident" if sign.is_none() && value.is_none() => return refuse(Attribute::Ident),
                b"working-tree-encoding" if sign.is_none() => return refuse(Attribute::WorkingTreeEncoding),
                b"binary" => return refuse(Attribute::Binary),
                _ => {}
            }
        }
    }
    Ok(auto)
}

/// A file's counts as git's convert.c `gather_stats` makes them.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct TextStats {
    /// CR immediately followed by LF (the pair counts once; its LF is not counted again).
    pub crlf: usize,
    /// CR not followed by LF.
    pub lone_cr: usize,
    pub nul: usize,
    /// Bytes from 32 to 126 and above 127, and BS, HT, ESC, FF.
    pub printable: usize,
    /// DEL (127) and every other byte below 32 but CR and LF, NUL included; a final Ctrl-Z
    /// (`\032`, an old end-of-file mark) is not counted.
    pub nonprintable: usize,
}

impl TextStats {
    /// convert.c `convert_is_binary`: a lone CR, a NUL, or more than one non-printable byte in
    /// 128 printable ones (`(printable >> 7) < nonprintable`).
    pub(crate) fn binary(&self) -> bool {
        self.lone_cr > 0 || self.nul > 0 || (self.printable >> 7) < self.nonprintable
    }
}

/// convert.c `gather_stats` over `bytes`. One pass, a work index, no recursion.
pub(crate) fn text_stats(bytes: &[u8]) -> TextStats {
    let mut stats = TextStats::default();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        at += 1;
        match byte {
            b'\r' => {
                if bytes.get(at) == Some(&b'\n') {
                    stats.crlf += 1;
                    at += 1;
                } else {
                    stats.lone_cr += 1;
                }
            }
            b'\n' => {}
            127 => stats.nonprintable += 1,
            // BS, HT, ESC, FF.
            0x08 | b'\t' | 0x1b | 0x0c => stats.printable += 1,
            0 => {
                stats.nul += 1;
                stats.nonprintable += 1;
            }
            1..=31 => stats.nonprintable += 1,
            _ => stats.printable += 1,
        }
    }
    // A final `\032` was counted as non-printable just above.
    if bytes.last() == Some(&0x1a) {
        stats.nonprintable -= 1;
    }
    stats
}

/// Whether `text=auto`'s clean may change `content`: it holds a CR LF pair and is not binary.
/// Only then is the index blob asked ([`blob_has_crlf`]); otherwise the raw bytes are hashed.
pub(crate) fn auto_may_convert(content: &[u8]) -> bool {
    let stats = text_stats(content);
    stats.crlf > 0 && !stats.binary()
}

/// convert.c `has_crlf_in_index` on the index entry's blob: it holds a CR, and its statistics
/// say text with at least one CR LF pair. Then `text=auto` hashes the file raw.
pub(crate) fn blob_has_crlf(blob: &[u8]) -> bool {
    if !blob.contains(&b'\r') {
        return false;
    }
    let stats = text_stats(blob);
    !stats.binary() && stats.crlf > 0
}
