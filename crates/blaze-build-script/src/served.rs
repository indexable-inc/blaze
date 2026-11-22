//! What of a checkout a build script is never served, and the planner never walks: the folders
//! at the checkout's root that are no package's. THE ONE HOME of that rule (rule 30).
//!
//! - blaze's own folder, `.blaze`, by its name: blaze makes it (the profile every command
//!   writes, `.blaze/v2/profile.txt`; blaze-cli clean.rs `CheckoutEntry`), so a run that read it
//!   would hold a read blaze itself moves ("changed while the command ran", turso c1851p).
//! - cargo's build folder, `target`, ONLY when it is one: cargo writes `CACHEDIR.TAG` in it
//!   (cargo `paths::create_dir_all_excluded_from_backups_atomically`), whose first line is the
//!   Cache Directory Tagging spec's signature, `Signature: 8a477f597d28d172789f06886806bc55`.
//!   A root folder named `target` without that tag is a package's folder like any other and is
//!   served (rule 4: no valid program is mis-served by a name guess).
//!
//! A folder not served is absent to a script: not listed, opened or looked at. The tag is read
//! wherever a path under `target` is answered, so each read's recorded answer is a function of
//! it: tagging the folder later moves the answer of every read that saw it (the root's listing,
//! a file in it), and the read list sees the change.

#![blaze::sans_io]

/// blaze's folder in the checkout's root.
pub const BLAZE_FOLDER: &str = ".blaze";

/// The name cargo's build folder has in the checkout's root (`Workspace::default_target_dir`).
pub const CARGO_TARGET: &str = "target";

/// The tag file cargo writes in its build folder.
pub const CACHEDIR_TAG: &str = "CACHEDIR.TAG";

/// The first line of a Cache Directory Tagging file (bford.info/cachedir: "the file must begin
/// with the following 43-byte header string").
pub const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// What a name at the checkout's root is to this rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RootName {
    /// blaze's folder: never served.
    Blaze,
    /// Named `target`: not served when its [`CACHEDIR_TAG`] is cargo's ([`tagged`]).
    Target,
    /// Any other name: served.
    Other,
}

/// What `name`, an entry of the checkout's root, is.
pub fn root_name(name: &[u8]) -> RootName {
    if name == BLAZE_FOLDER.as_bytes() {
        RootName::Blaze
    } else if name == CARGO_TARGET.as_bytes() {
        RootName::Target
    } else {
        RootName::Other
    }
}

/// Whether the bytes of `target/CACHEDIR.TAG` (`None`: no such file) mark a cache folder: they
/// begin with [`CACHEDIR_SIGNATURE`]. The spec's header is the line's whole first 43 bytes; what
/// follows it is the tag's own text.
pub fn tagged(tag: Option<&[u8]>) -> bool {
    tag.is_some_and(|bytes| bytes.starts_with(CACHEDIR_SIGNATURE))
}

/// Whether the root entry `name` is served, `tag` being the bytes of `target/CACHEDIR.TAG` (the
/// first [`CACHEDIR_SIGNATURE`]`.len()` are enough; read only when [`root_name`] is
/// [`RootName::Target`], `None` otherwise).
pub fn served_at_root(name: &[u8], tag: Option<&[u8]>) -> bool {
    match root_name(name) {
        RootName::Blaze => false,
        RootName::Target => !tagged(tag),
        RootName::Other => true,
    }
}
