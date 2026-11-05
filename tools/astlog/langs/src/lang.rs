use std::path::Path;

use crate::{profiles, types::Profile};

/// The grammars blaze lints: its code, its manifests and configs, and its scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Toml,
    Python,
}

impl Lang {
    #[must_use]
    pub fn to_tree_sitter(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
        }
    }

    #[must_use]
    pub fn profile(self) -> &'static Profile {
        profiles::get(self)
    }

    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Rust, Self::Toml, Self::Python]
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        self.profile().name
    }
}

#[must_use]
pub fn detect(path: &Path) -> Option<Lang> {
    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
        for lang in Lang::all() {
            if lang.profile().file_names.contains(&file_name) {
                return Some(*lang);
            }
        }
    }

    let ext = path.extension()?.to_str()?;
    for lang in Lang::all() {
        if lang.profile().extensions.contains(&ext) {
            return Some(*lang);
        }
    }

    None
}

#[must_use]
pub fn detect_from_extension(ext: &str) -> Option<Lang> {
    let ext = ext.strip_prefix('.').unwrap_or(ext);
    for lang in Lang::all() {
        if lang.profile().extensions.contains(&ext) {
            return Some(*lang);
        }
    }
    None
}
