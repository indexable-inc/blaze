//! The part of `compiler/rustc_expand/src/module.rs` at rust-lang/rust 12433c307 (MIT or
//! Apache-2.0) rustfmt calls: where `mod foo;` finds its file.

use std::path::{self, Path, PathBuf};

use crate::span::Ident;

#[derive(Copy, Clone)]
pub(crate) enum DirOwnership {
    Owned {
        // None if `mod.rs`, `Some("foo")` if we're in `foo.rs`.
        relative: Option<Ident>,
    },
    UnownedViaBlock,
}

// Public for rustfmt usage.
pub(crate) struct ModulePathSuccess {
    pub(crate) file_path: PathBuf,
    pub(crate) dir_ownership: DirOwnership,
}

/// Why `default_submod_path` found no file. rustc's `ModError` also has the errors only its
/// expansion makes (circular inclusion, `mod` in a block, a module that failed to parse).
pub(crate) enum ModError {
    FileNotFound(PathBuf, PathBuf),
    MultipleCandidates(PathBuf, PathBuf),
}

/// Returns a path to a module.
// Public for rustfmt usage.
pub(crate) fn default_submod_path(
    ident: Ident,
    relative: Option<Ident>,
    dir_path: &Path,
) -> Result<ModulePathSuccess, ModError> {
    // If we're in a foo.rs file instead of a mod.rs file,
    // we need to look for submodules in
    // `./foo/<ident>.rs` and `./foo/<ident>/mod.rs` rather than
    // `./<ident>.rs` and `./<ident>/mod.rs`.
    let relative_prefix_string;
    let relative_prefix = if let Some(ident) = relative {
        relative_prefix_string = format!("{}{}", ident.name, path::MAIN_SEPARATOR);
        &relative_prefix_string
    } else {
        ""
    };

    let default_path_str = format!("{}{}.rs", relative_prefix, ident.name);
    let secondary_path_str = format!(
        "{}{}{}mod.rs",
        relative_prefix,
        ident.name,
        path::MAIN_SEPARATOR
    );
    let default_path = dir_path.join(&default_path_str);
    let secondary_path = dir_path.join(&secondary_path_str);
    let default_exists = default_path.exists();
    let secondary_exists = secondary_path.exists();

    match (default_exists, secondary_exists) {
        (true, false) => Ok(ModulePathSuccess {
            file_path: default_path,
            dir_ownership: DirOwnership::Owned {
                relative: Some(ident),
            },
        }),
        (false, true) => Ok(ModulePathSuccess {
            file_path: secondary_path,
            dir_ownership: DirOwnership::Owned { relative: None },
        }),
        (false, false) => Err(ModError::FileNotFound(default_path, secondary_path)),
        (true, true) => Err(ModError::MultipleCandidates(default_path, secondary_path)),
    }
}
