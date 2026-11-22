//! The schema's closed sets of names: each a number on the wire (CBOR) and a name in JSON, one
//! table each, so the two renderings cannot disagree (tools/world/schema.ts holds the same
//! tables, index = code).

macro_rules! codes {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $code:literal, $json:literal;)* }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)*
        }

        impl $name {
            /// Every value, in the order of its code.
            pub const ALL: &'static [$name] = &[$($name::$variant,)*];

            /// Its number on the wire.
            pub fn code(self) -> u64 {
                match self {
                    $($name::$variant => $code,)*
                }
            }

            /// The value of a number on the wire, if the schema has one.
            pub fn from_code(code: u64) -> Option<$name> {
                match code {
                    $($code => Some($name::$variant),)*
                    _ => None,
                }
            }

            /// Its name in the JSON rendering.
            pub fn name(self) -> &'static str {
                match self {
                    $($name::$variant => $json,)*
                }
            }
        }
    };
}

codes! {
    /// Which call a request makes: key 1 of a request.
    Call {
        Open = 0, "open";
        Patch = 1, "patch";
        Read = 2, "read";
        Check = 3, "check";
        Diff = 4, "diff";
        Commit = 5, "commit";
        Explain = 6, "explain";
    }
}

codes! {
    /// Why a patch's edit was refused.
    EditWhy {
        /// The file is not in the base world (or an earlier edit of the patch deleted it).
        NoSuchFile = 0, "no_such_file";
        /// A rename's target is a file of the world already.
        Exists = 1, "exists";
        /// A range past the end of the file, or one whose start is past its end.
        OutOfBounds = 2, "out_of_bounds";
        /// A range's start or end inside a UTF-8 character.
        NotCharBoundary = 3, "not_char_boundary";
        /// The bytes in the range are not the ones the edit says it replaces.
        ExpectMismatch = 4, "expect_mismatch";
        /// No item of the world has the key.
        NoSuchItem = 5, "no_such_item";
        /// Two ranges of the patch in one file overlap.
        Overlap = 6, "overlap";
        /// A range or item edit of a file an earlier edit of the patch wrote, deleted or renamed:
        /// its offsets are the base world's, which that file no longer is.
        Conflict = 7, "conflict";
    }
}

codes! {
    /// What was wrong with bytes: the code of a [`crate::WireError`] and of a wire refusal.
    WireCode {
        /// A CBOR rule other than those below, or another kind of item than the schema has here.
        Cbor = 0, "cbor";
        /// A number or a length not in the shortest head that holds it.
        NotShortest = 1, "not_shortest";
        /// An indefinite or reserved length.
        Indefinite = 2, "indefinite";
        /// A map key the schema does not have for this shape (`said`: the key).
        UnknownKey = 3, "unknown_key";
        /// A map key twice (`said`: the key).
        DuplicateKey = 4, "duplicate_key";
        /// A map key below the one before it (`said`: the key).
        KeyOrder = 5, "key_order";
        /// A key the shape must have is not there (`said`: the key).
        MissingKey = 6, "missing_key";
        /// A call, reply or variant tag the schema does not have (`said`: the tag).
        UnknownVariant = 7, "unknown_variant";
        /// A message of another version of the schema (`said`: its version).
        Version = 8, "version";
        /// A text past its limit (`said`: the limit).
        TooLong = 9, "too_long";
        /// A list past its limit (`said`: the limit).
        TooMany = 10, "too_many";
        /// A message past the limit on bytes (`said`: the limit).
        TooLarge = 11, "too_large";
        /// Arrays and maps nested past the schema's depth (`said`: the limit).
        TooDeep = 12, "too_deep";
        /// A path that is not relative, has an empty, `.` or `..` part, or holds NUL or `\`; or a
        /// checkout that is not absolute.
        BadPath = 13, "bad_path";
        /// An integer past [`crate::MOST_INT`], an id not of 32 bytes, or a range whose start is
        /// past its end.
        Range = 14, "range";
        /// Bytes after the first message of a connection: one request a connection.
        SecondRequest = 15, "second_request";
        /// The connection ended before a whole message.
        Short = 16, "short";
        /// Bytes after a message given to decode alone.
        Trailing = 17, "trailing";
    }
}

codes! {
    /// What a listed name is.
    EntryKind {
        File = 0, "file";
        Dir = 1, "dir";
    }
}
