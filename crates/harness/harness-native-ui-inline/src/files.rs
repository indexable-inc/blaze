//! File kinds (den's filetype.rs), the one table an inline `<Path>`, a cell's `FileRef` and a cell's `Icon` share: each
//! kind's Nerd Font glyph, coloured by the scene's `hn.GenFileKind.<kind>` (`tone_file_<kind>`).

/// The file kinds and each one's Nerd Font glyph.
pub const KINDS: [(&str, char); 20] = [
    ("rust", '\u{E7A8}'),
    ("typescript", '\u{E628}'),
    ("tsx", '\u{E628}'),
    ("javascript", '\u{E60C}'),
    ("json", '\u{EB0F}'),
    ("toml", '\u{E615}'),
    ("yaml", '\u{E6A8}'),
    ("markdown", '\u{E609}'),
    ("nix", '\u{F313}'),
    ("shell", '\u{E691}'),
    ("python", '\u{E606}'),
    ("go", '\u{E627}'),
    ("html", '\u{E60E}'),
    ("css", '\u{E614}'),
    ("image", '\u{EAEA}'),
    ("lock", '\u{EA75}'),
    ("git", '\u{EA68}'),
    ("folder", '\u{EA83}'),
    ("folder_open", '\u{EAF7}'),
    ("text", '\u{EA7B}'),
];

/// The kinds' names, in [`KINDS`]'s order (an error lists them).
pub const KIND_NAMES: [&str; 20] = {
    let mut out = [""; 20];
    let mut i = 0;
    while i < 20 {
        out[i] = KINDS[i].0;
        i += 1;
    }
    out
};

/// The file kind an Icon's `name` says (a kind, or den's other names for one: `rs`, `ts`, `md`, `dir`, ...).
#[must_use]
pub fn kind_named(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    let k = match n.as_str() {
        "rs" => "rust",
        "ts" => "typescript",
        "js" | "jsx" | "mjs" => "javascript",
        "md" => "markdown",
        "yml" => "yaml",
        "sh" | "bash" | "zsh" => "shell",
        "py" => "python",
        "package" => "json",
        "picture" | "photo" | "png" | "jpg" | "svg" => "image",
        "dir" | "directory" => "folder",
        "folder-open" => "folder_open",
        "file" | "generic" | "doc" | "txt" => "text",
        other => other,
    };
    KIND_NAMES.iter().copied().find(|x| *x == k)
}

/// The file kind of the file at `path`, by its name (den's `Kind::of`): a folder for a trailing `/`, a few names of their
/// own, else its extension; plain text when none says.
#[must_use]
pub fn kind_of(path: &str) -> &'static str {
    if path.ends_with('/') {
        return "folder";
    }
    let base = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    match base.as_str() {
        "flake.nix" | "default.nix" | "shell.nix" => return "nix",
        "cargo.lock" | "flake.lock" | "package-lock.json" | "yarn.lock" | "bun.lock" | "bun.lockb" | "pnpm-lock.yaml" | "uv.lock" | "poetry.lock" => {
            return "lock";
        }
        ".gitignore" | ".gitattributes" | ".gitmodules" => return "git",
        ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".zprofile" | ".zshenv" => return "shell",
        _ => {}
    }
    let ext = base.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "json" | "jsonc" | "json5" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "mdx" | "markdown" => "markdown",
        "nix" => "nix",
        "sh" | "bash" | "zsh" | "fish" => "shell",
        "py" | "pyi" => "python",
        "go" => "go",
        "html" | "htm" => "html",
        "css" | "scss" | "sass" | "less" => "css",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "bmp" | "tiff" | "avif" | "heic" => "image",
        "lock" => "lock",
        _ => "text",
    }
}

/// A kind's glyph (plain text's for a kind not in [`KINDS`]).
#[must_use]
pub fn kind_glyph(kind: &str) -> char {
    KINDS.iter().find(|(k, _)| *k == kind).map_or('\u{EA7B}', |(_, g)| *g)
}
