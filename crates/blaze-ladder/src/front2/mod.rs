//! The new front end of the ladder's runner (README, "For a successor lane (03:30)"): the
//! checker's `World` over every crate's names as the names library (blaze-scope) resolved them.
//! It holds no lookup rule of its own (rule 30): every path goes through [`crate::resolve`],
//! every impl and lang item comes from the library's tables.
//!
//! | module | what |
//! |---|---|
//! | [`texts`] | the one text table: the user's files first, then every crate's files and expansions |
//! | [`keys`] | the one map between library keys (32 bytes) and checker keys (16 bytes); members' keys |
//! | [`index`] | every impl and lang item, from the library's tables |
//! | [`types`] | a written type to a portable one through the resolver; `Why::Need` for a head not lowered yet |

pub mod body;
pub mod buckets;
pub mod check;
pub mod consts;
pub mod dyn_ty;
pub mod entry;
pub mod fill;
pub mod heads;
pub mod index;
pub mod keys;
pub mod langs;
pub mod pattern_ty;
pub mod preds;
pub mod program;
pub mod reach;
pub mod spell;
pub mod texts;
pub mod types;
pub mod world_items;
pub mod world_paths;

use blaze_scope::resolve::Names;

use crate::refusal::SourceFile;
use crate::resolve::Resolver;

/// The text appended to the program's root: the module `__blaze_entry`, whose hosted start
/// calls `crate::main` with the roots `fn main(cx: &Cx, io: Io)` takes (docs/v2/plan/design/
/// std.md, the entry; bstd's `guest::start` and `guest::finish`).
pub const ENTRY_CX_IO: &str = "\nmod __blaze_entry {\n    #[unsafe(no_mangle)]\n    pub extern \"C\" fn bstd_hosted_start(table: *const std::wire::HostTable) -> i32 {\n        fn hosted(cx: &std::Cx, io: std::Io) -> std::entry::Status {\n            let ran = crate::main(cx, io.clone());\n            std::guest::finish(&io, ran)\n        }\n        std::guest::start(table, hosted)\n    }\n}\n";

/// [`ENTRY_CX_IO`] for `fn main()` (D59's interim, [`entry`]).
pub const ENTRY_NONE: &str = "\nmod __blaze_entry {\n    #[unsafe(no_mangle)]\n    pub extern \"C\" fn bstd_hosted_start(table: *const std::wire::HostTable) -> i32 {\n        fn hosted(_cx: &std::Cx, io: std::Io) -> std::entry::Status {\n            let ran = crate::main();\n            std::guest::finish(&io, ran)\n        }\n        std::guest::start(table, hosted)\n    }\n}\n";

/// [`ENTRY_CX_IO`] for `fn main(cx: &Cx)`.
pub const ENTRY_CX: &str = "\nmod __blaze_entry {\n    #[unsafe(no_mangle)]\n    pub extern \"C\" fn bstd_hosted_start(table: *const std::wire::HostTable) -> i32 {\n        fn hosted(cx: &std::Cx, io: std::Io) -> std::entry::Status {\n            let ran = crate::main(cx);\n            std::guest::finish(&io, ran)\n        }\n        std::guest::start(table, hosted)\n    }\n}\n";

/// [`ENTRY_CX_IO`] for `fn main(io: Io)`.
pub const ENTRY_IO: &str = "\nmod __blaze_entry {\n    #[unsafe(no_mangle)]\n    pub extern \"C\" fn bstd_hosted_start(table: *const std::wire::HostTable) -> i32 {\n        fn hosted(_cx: &std::Cx, io: std::Io) -> std::entry::Status {\n            let ran = crate::main(io.clone());\n            std::guest::finish(&io, ran)\n        }\n        std::guest::start(table, hosted)\n    }\n}\n";

/// Why the front end could not start.
#[derive(Debug)]
pub enum FrontError {
    Texts(texts::TextsError),
    Key(keys::KeyClash),
    Lang(index::LangTwice),
}

impl core::fmt::Display for FrontError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FrontError::Texts(e) => write!(f, "{e}"),
            FrontError::Key(e) => write!(f, "{e}"),
            FrontError::Lang(e) => write!(f, "{e}"),
        }
    }
}

/// The front end over one program's names.
pub struct Front2<'n> {
    pub names: &'n Names<'n>,
    pub resolver: Resolver<'n>,
    pub texts: texts::Texts<'n>,
    pub keys: keys::Keys,
    pub index: index::Index<'n>,
}

impl<'n> Front2<'n> {
    /// The front end over `names`, the program's crate at index 0 and its files `user` as the
    /// caller gave them. Every item, variant and impl the library keys is filed in the key map
    /// here, so a clash stops the program before any item is lowered.
    pub fn new(names: &'n Names<'n>, user: &[SourceFile<'_>]) -> Result<Front2<'n>, FrontError> {
        let texts = texts::Texts::new(names, user).map_err(FrontError::Texts)?;
        let index = index::Index::new(names).map_err(FrontError::Lang)?;
        let mut keys = keys::Keys::new();
        for (krate, c) in names.crates().iter().enumerate() {
            for (key, _, _) in names.items(krate) {
                keys.item(key).map_err(FrontError::Key)?;
            }
            for vs in c.tables.enums.values() {
                for v in vs.iter() {
                    keys.item(v.key).map_err(FrontError::Key)?;
                }
            }
            for m in names.modules(krate) {
                keys.item(m.key).map_err(FrontError::Key)?;
            }
        }
        for i in index.impls.iter() {
            keys.item(i.key).map_err(FrontError::Key)?;
        }
        Ok(Front2 { names, resolver: Resolver { names }, texts, keys, index })
    }
}
