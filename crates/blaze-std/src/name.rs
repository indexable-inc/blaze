//! Typed names (docs/v2/plan/design/std.md, "Names are typed, and authority is stored"): what a
//! capability's methods take in place of a string. Each is checked as it is made by one rule
//! ([`refused`]): 1 to [`NAME_MOST`] bytes, no control byte, no space at either end. A field's
//! type then says which capability the name is for.

use alloc::{borrow::ToOwned, string::String, sync::Arc};
use core::fmt;

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
}

impl NameWhy {
    const fn says(self) -> &'static str {
        match self {
            NameWhy::Empty => "is empty",
            NameWhy::TooLong => "is longer than 255 bytes",
            NameWhy::Control => "holds a control byte",
            NameWhy::Padded => "starts or ends with a space",
        }
    }
}

/// What a typed name's `new` refused: which kind of name, the text and why.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct NameError {
    /// The kind of name, as its type is called: `KeyName`, `DeviceName`, ...
    pub kind: &'static str,
    pub name: String,
    pub why: NameWhy,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:?} {}", self.kind, self.name, self.why.says())
    }
}

impl core::error::Error for NameError {}

/// Why `raw` is no name, if it is not: the one rule of every typed name here.
#[blaze::pure]
pub const fn refused(raw: &str) -> Option<NameWhy> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return Some(NameWhy::Empty);
    }
    if bytes.len() > NAME_MOST {
        return Some(NameWhy::TooLong);
    }
    if bytes[0] == b' ' || bytes[bytes.len() - 1] == b' ' {
        return Some(NameWhy::Padded);
    }
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] < 0x20 || bytes[at] == 0x7f {
            return Some(NameWhy::Control);
        }
        at += 1;
    }
    None
}

/// One typed name: an `Arc<str>` checked by [`refused`] as it is made.
macro_rules! typed_name {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)] // clone: a refcount, no allocation
        pub struct $name(Arc<str>);

        impl $name {
            pub fn new(raw: &str) -> Result<$name, NameError> {
                if let Some(why) = refused(raw) {
                    // alloc: an error's message.
                    return Err(NameError { kind: stringify!($name), name: raw.to_owned(), why });
                }
                // alloc: once per name, its text.
                Ok($name(Arc::from(raw)))
            }

            pub fn as_str(&self) -> &str {
                &self.0
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
