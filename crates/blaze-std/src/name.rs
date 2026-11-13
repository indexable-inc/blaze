//! Typed names (docs/v2/plan/design/std.md, "Names are typed, and authority is stored"): what a
//! capability's methods take in place of a string. Each is checked as it is made by one rule
//! ([`refused`]): 1 to [`NAME_MOST`] bytes, no control byte, no space at either end. A field's
//! type then says which capability the name is for.

use alloc::{string::String, sync::Arc};
use core::fmt::{self, Write as _};

use crate::mem::{OutOfMemory, heap};

/// The most bytes in a name.
pub const NAME_MOST: usize = 255;

/// Why a name was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NameWhy {
    Empty,
    /// More than [`NAME_MOST`] bytes.
    TooLong,
    /// A control byte (below 0x20, or 0x7f).
    Control,
    /// A space at its start or its end.
    Padded,
    /// It passed, and there was no room to keep it.
    OutOfMemory(OutOfMemory),
}

impl NameWhy {
    const fn says(self) -> &'static str {
        match self {
            NameWhy::Empty => "is empty",
            NameWhy::TooLong => "is longer than 255 bytes",
            NameWhy::Control => "holds a control byte",
            NameWhy::Padded => "starts or ends with a space",
            NameWhy::OutOfMemory(_) => "could not be kept: the global heap had no room",
        }
    }
}

/// What a typed name's `new` refused: which kind of name, the text and why.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct NameError {
    /// The kind of name, as its type is called: `KeyName`, `DeviceName`, ...
    pub kind: &'static str,
    /// The text refused, or its start when it is long: kept in the error, which allocates
    /// nothing.
    pub name: Excerpt,
    pub why: NameWhy,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:?} {}", self.kind, self.name, self.why.says())?;
        match self.why {
            NameWhy::OutOfMemory(error) => write!(f, " ({error})"),
            _ => Ok(()),
        }
    }
}

impl core::error::Error for NameError {}

/// The most bytes of a text an [`Excerpt`] keeps.
pub const EXCERPT_MOST: usize = 64;

/// The text an error is about, kept in the error itself: its first [`EXCERPT_MOST`] bytes
/// (cut at a character's end) and how long the whole was. An error never allocates its
/// message (docs/v2/plan/design/fallible-bstd.md): this is how one names a text it was given
/// without holding a copy on the heap. It is also where text is written to
/// (`write!(excerpt, ..)`, which never fails: what does not fit is counted, not kept).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Excerpt {
    bytes: [u8; EXCERPT_MOST],
    /// How many of `bytes` hold the text: at most [`EXCERPT_MOST`].
    kept: usize,
    /// How many bytes the whole text had.
    whole: usize,
}

impl Excerpt {
    /// No text yet.
    pub const EMPTY: Excerpt = Excerpt { bytes: [0; EXCERPT_MOST], kept: 0, whole: 0 };

    /// The start of `text`.
    pub fn of(text: &str) -> Excerpt {
        Excerpt::of_parts(&[text])
    }

    /// The start of `value`'s `Debug` form. A `Debug` that fails says so at the end of what it
    /// wrote: an excerpt itself takes every write.
    pub fn debug(value: &impl fmt::Debug) -> Excerpt {
        let mut excerpt = Excerpt::EMPTY;
        if write!(excerpt, "{value:?}").is_err() {
            excerpt.add(" (its Debug failed here)");
        }
        excerpt
    }

    /// The start of `parts` written one after another.
    pub fn of_parts(parts: &[&str]) -> Excerpt {
        let mut excerpt = Excerpt::EMPTY;
        for part in parts {
            excerpt.add(part);
        }
        excerpt
    }

    /// `text` after what is kept, as much of it as fits; once a text was cut nothing more is
    /// kept, so what is kept is always a prefix of the whole.
    fn add(&mut self, text: &str) {
        let kept = self.kept;
        let cut = self.whole > kept;
        self.whole = self.whole.saturating_add(text.len());
        if cut {
            return;
        }
        let mut fit = text.len().min(EXCERPT_MOST.saturating_sub(kept));
        while !text.is_char_boundary(fit) {
            fit -= 1;
        }
        for (slot, byte) in self.bytes.iter_mut().skip(kept).zip(text.as_bytes().iter().take(fit)) {
            *slot = *byte;
        }
        self.kept = kept + fit;
    }

    /// How many bytes the whole text had.
    pub fn whole(&self) -> usize {
        self.whole
    }

    /// Whether the text was longer than what is kept.
    pub fn is_cut(&self) -> bool {
        self.whole > self.kept
    }

    /// The bytes kept: whole characters of the text.
    fn kept(&self) -> Result<&[u8], fmt::Error> {
        match self.bytes.split_at_checked(self.kept) {
            Some((kept, _)) => Ok(kept),
            None => Err(fmt::Error),
        }
    }
}

impl fmt::Write for Excerpt {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.add(text);
        Ok(())
    }
}

impl fmt::Display for Excerpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.kept()?.utf8_chunks() {
            f.write_str(chunk.valid())?;
        }
        if self.is_cut() {
            write!(f, "... ({} bytes)", self.whole)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Excerpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('"')?;
        for chunk in self.kept()?.utf8_chunks() {
            for ch in chunk.valid().chars() {
                write!(f, "{}", ch.escape_debug())?;
            }
        }
        f.write_char('"')?;
        if self.is_cut() {
            write!(f, "... ({} bytes)", self.whole)?;
        }
        Ok(())
    }
}

/// Why `raw` is no name, if it is not: the one rule of every typed name here.
pub const fn refused(raw: &str) -> Option<NameWhy> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return Some(NameWhy::Empty);
    }
    if bytes.len() > NAME_MOST {
        return Some(NameWhy::TooLong);
    }
    if let [b' ', ..] | [.., b' '] = bytes {
        return Some(NameWhy::Padded);
    }
    let mut rest = bytes;
    while let [byte, tail @ ..] = rest {
        if *byte < 0x20 || *byte == 0x7f {
            return Some(NameWhy::Control);
        }
        rest = tail;
    }
    None
}

/// One typed name: its text on the global heap behind an `Arc`, checked by [`refused`] as it
/// is made. `alloc` has no fallible `Arc<str>` from a `str`, so the text is a `String` in a
/// fallible `Arc`: two allocations, each able to say it had no room.
macro_rules! typed_name {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)] // clone: a refcount, no allocation
        pub struct $name(Arc<String>);

        impl $name {
            pub fn new(raw: &str) -> Result<$name, NameError> {
                let refuse = |why| NameError { kind: stringify!($name), name: Excerpt::of(raw), why };
                if let Some(why) = refused(raw) {
                    return Err(refuse(why));
                }
                let out = |error| refuse(NameWhy::OutOfMemory(error));
                // alloc: once per name, its text, shared by its clones.
                let text = heap::string(raw).map_err(out)?;
                // alloc: once per name, its count.
                Ok($name(heap::shared(text).map_err(out)?))
            }

            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:?})"), self.as_str())
            }
        }

        impl TryFrom<&str> for $name {
            type Error = NameError;

            fn try_from(raw: &str) -> Result<$name, NameError> {
                $name::new(raw)
            }
        }
    };
}

typed_name! {
    /// A key in the OS keychain or in hardware: what [`crate::keys::Keys`] takes.
    KeyName
}
typed_name! {
    /// A certificate authority: what [`crate::roots::Roots`] is a set of.
    Authority
}
typed_name! {
    /// A time zone, as the zone database names it (`Europe/Paris`).
    ZoneName
}
typed_name! {
    /// A language tag (`en-GB`).
    Language
}
typed_name! {
    /// A user's login name.
    UserName
}
typed_name! {
    /// A module of native code: what [`crate::code::Code`] loads.
    ModuleName
}
typed_name! {
    /// An application another one drives: what [`crate::automation::Automation`] takes.
    AppName
}
typed_name! {
    /// A device: a sound card, a camera, a display, a serial port.
    DeviceName
}
typed_name! {
    /// A service a Bluetooth device offers.
    ServiceName
}
