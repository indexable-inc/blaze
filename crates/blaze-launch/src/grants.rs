//! `BLAZE_LAUNCH_GRANTS` (docs/v2/plan/design/run.md, "Every program runs in the child"): the
//! grants `blaze run`'s command line gave a hosted program, handed from the parent to the
//! launcher, which makes the program's host (bstd `Hosting`) in the child with them. The one home
//! of the variable's form: the parent writes it with [`format`], the launcher reads it with
//! [`parse`], and nothing else spells it.
//!
//! **The form.** Each grant is the host's `(kind, name)` pair (blaze-cli host_grants.rs
//! `host_grant`: the kind as bstd's guest names it on the wire, `Kind::name`, then the
//! parameter's name), written `<kind>=<name>`; the grants are joined by `,`, in the order the
//! command line gave them. No grant is the empty text. Each part is 1 to [`MOST_PART`] bytes of
//! ASCII letters, digits, `_` and `-` (every kind bstd names and every parameter name a
//! blaze-edition `main` takes today); there are at most [`MOST_GRANTS`] grants and the whole is
//! at most [`MOST_BYTES`] bytes. A pair given twice is refused: the list is a set the host asks
//! of, so a second copy means the parent is out of step.
//!
//! Both sides refuse what is not of the form with a [`GrantsError`] naming the grant (counted
//! from 1) and its part: [`format`] before a byte reaches the child, [`parse`] before the
//! program starts. Nothing is skipped or guessed (rule 3).

use core::fmt;

/// The variable's name.
pub const VAR: &str = "BLAZE_LAUNCH_GRANTS";
/// The most grants one run is given (rule 18).
pub const MOST_GRANTS: usize = 64;
/// The most bytes of one kind or one name.
pub const MOST_PART: usize = 64;
/// The most bytes of the whole value: every grant at its longest, with its `=` and `,`.
pub const MOST_BYTES: usize = MOST_GRANTS * (2 * MOST_PART + 2);

/// Which part of a grant a refusal names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Part {
    Kind,
    Name,
}

impl Part {
    fn word(self) -> &'static str {
        match self {
            Part::Kind => "kind",
            Part::Name => "name",
        }
    }
}

/// Why a grants value cannot be written or read. `nth` counts grants from 1.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GrantsError {
    /// More than [`MOST_GRANTS`] grants.
    TooMany { most: usize },
    /// The value is `len` bytes, more than [`MOST_BYTES`].
    TooLong { len: usize, most: usize },
    /// The `nth` grant has no `=` between its kind and its name.
    NoEquals { nth: usize },
    /// The `nth` grant's `part` is empty.
    Empty { nth: usize, part: Part },
    /// The `nth` grant's `part` is longer than [`MOST_PART`] bytes.
    PartTooLong { nth: usize, part: Part, len: usize },
    /// The `nth` grant's `part` holds byte `byte`, which is no ASCII letter, digit, `_` or `-`.
    Character { nth: usize, part: Part, byte: u8 },
    /// The `nth` grant is the same pair as the `first`.
    Twice { nth: usize, first: usize },
    /// The value is not UTF-8.
    NotText,
}

impl fmt::Display for GrantsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{VAR} (`<kind>=<name>` pairs joined by `,`): ")?;
        match *self {
            GrantsError::TooMany { most } => write!(f, "more than {most} grants"),
            GrantsError::TooLong { len, most } => write!(f, "{len} bytes, more than {most}"),
            GrantsError::NoEquals { nth } => write!(f, "grant {nth} has no `=` between its kind and its name"),
            GrantsError::Empty { nth, part } => write!(f, "grant {nth}'s {} is empty", part.word()),
            GrantsError::PartTooLong { nth, part, len } => {
                write!(f, "grant {nth}'s {} is {len} bytes, more than {MOST_PART}", part.word())
            }
            GrantsError::Character { nth, part, byte } => write!(
                f,
                "grant {nth}'s {} holds byte 0x{byte:02x}, which is no ASCII letter, digit, `_` or `-`",
                part.word()
            ),
            GrantsError::Twice { nth, first } => write!(f, "grant {nth} is the same pair as grant {first}"),
            GrantsError::NotText => f.write_str("the value is not UTF-8"),
        }
    }
}

impl core::error::Error for GrantsError {}

/// Whether `byte` may be in a kind or a name.
fn allowed(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// Checks one part of the `nth` grant.
fn check_part(nth: usize, part: Part, text: &[u8]) -> Result<(), GrantsError> {
    if text.is_empty() {
        return Err(GrantsError::Empty { nth, part });
    }
    if text.len() > MOST_PART {
        return Err(GrantsError::PartTooLong { nth, part, len: text.len() });
    }
    match text.iter().find(|&&byte| !allowed(byte)) {
        Some(&byte) => Err(GrantsError::Character { nth, part, byte }),
        None => Ok(()),
    }
}

/// One grant's place in the value: its kind's and its name's byte ranges.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Spans {
    kind: (u16, u16),
    name: (u16, u16),
}

/// A grants value read whole and checked: its pairs in the order given.
#[derive(Clone, Copy, Debug)]
pub struct Grants<'t> {
    text: &'t str,
    spans: [Spans; MOST_GRANTS],
    len: usize,
}

impl<'t> Grants<'t> {
    /// How many grants.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Each `(kind, name)`, in the order given.
    pub fn iter(&self) -> impl Iterator<Item = (&'t str, &'t str)> + '_ {
        let text = self.text;
        // Every range was checked inside `text`, on ASCII bytes, by `parse`.
        self.spans[..self.len].iter().map(move |spans| {
            let at = |(from, to): (u16, u16)| &text[from as usize..to as usize];
            (at(spans.kind), at(spans.name))
        })
    }
}

/// Reads the launcher's `BLAZE_LAUNCH_GRANTS` value: the empty text is no grant; anything not of
/// the module's form is refused whole, naming the first grant that is not.
pub fn parse(value: &[u8]) -> Result<Grants<'_>, GrantsError> {
    if value.len() > MOST_BYTES {
        return Err(GrantsError::TooLong { len: value.len(), most: MOST_BYTES });
    }
    // Text first; every byte is then checked to be ASCII (a part's) or a separator, so each
    // span below starts and ends on a character boundary.
    let text = core::str::from_utf8(value).map_err(|_| GrantsError::NotText)?;
    let mut spans = [Spans::default(); MOST_GRANTS];
    let mut len = 0;
    if !value.is_empty() {
        let mut from = 0usize;
        for (index, pair) in value.split(|&byte| byte == b',').enumerate() {
            let nth = index + 1;
            if index >= MOST_GRANTS {
                return Err(GrantsError::TooMany { most: MOST_GRANTS });
            }
            let Some(equals) = pair.iter().position(|&byte| byte == b'=') else {
                return Err(GrantsError::NoEquals { nth });
            };
            let (kind, name) = (&pair[..equals], &pair[equals + 1..]);
            check_part(nth, Part::Kind, kind)?;
            // A second `=` is a byte the name may not hold, refused here.
            check_part(nth, Part::Name, name)?;
            for (earlier, seen) in spans[..len].iter().enumerate() {
                let at = |(a, b): (u16, u16)| &value[a as usize..b as usize];
                if at(seen.kind) == kind && at(seen.name) == name {
                    return Err(GrantsError::Twice { nth, first: earlier + 1 });
                }
            }
            // Bounded by MOST_BYTES, which fits a u16.
            let kind_at = from as u16;
            let name_at = (from + equals + 1) as u16;
            let end = (from + pair.len()) as u16;
            spans[len] = Spans { kind: (kind_at, kind_at + kind.len() as u16), name: (name_at, end) };
            len += 1;
            from += pair.len() + 1;
        }
    }
    Ok(Grants { text, spans, len })
}

/// Writes `grants`, the host's `(kind, name)` pairs in the command line's order, as the value of
/// `BLAZE_LAUNCH_GRANTS`, into `out`: the text written, which [`parse`] reads back as the same
/// pairs. A pair not of the module's form is refused before anything is handed to the child.
pub fn format<'o>(grants: &[(&str, &str)], out: &'o mut [u8; MOST_BYTES]) -> Result<&'o str, GrantsError> {
    if grants.len() > MOST_GRANTS {
        return Err(GrantsError::TooMany { most: MOST_GRANTS });
    }
    let mut len = 0usize;
    for (index, &(kind, name)) in grants.iter().enumerate() {
        let nth = index + 1;
        check_part(nth, Part::Kind, kind.as_bytes())?;
        check_part(nth, Part::Name, name.as_bytes())?;
        if let Some(first) = grants[..index].iter().position(|&seen| seen == (kind, name)) {
            return Err(GrantsError::Twice { nth, first: first + 1 });
        }
        // Fits: at most MOST_GRANTS pairs of at most MOST_PART bytes each, with `=` and `,`.
        if index > 0 {
            out[len] = b',';
            len += 1;
        }
        out[len..len + kind.len()].copy_from_slice(kind.as_bytes());
        len += kind.len();
        out[len] = b'=';
        len += 1;
        out[len..len + name.len()].copy_from_slice(name.as_bytes());
        len += name.len();
    }
    // Every byte written is a checked part's ASCII or a separator.
    core::str::from_utf8(&out[..len]).map_err(|_| GrantsError::NotText)
}
