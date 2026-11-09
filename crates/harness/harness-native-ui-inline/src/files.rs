//! Which icon a file or folder is drawn with, the one home for it: a place's kind by its name (special names first, then
//! its extension) or, for a folder, by the folder's name, and each kind's drawing, one of harness-native-render's
//! drawings (icons/files: IntelliJ's New UI file icons, in their own colours). An inline `<Path>` and `<FileRef>`, a path's
//! hover card and its folder's entries, a cell's `File`, `FileRef` and `Icon` and a step's file chip all draw through it.

/// The file kinds and the drawing each is shown with (a drawing's name is its file's stem under icons/files).
pub const KINDS: [(&str, &str); 40] = [
    ("rust", "rust"),
    ("typescript", "typeScript"),
    ("tsx", "typeScript"),
    ("javascript", "javaScript"),
    ("json", "json"),
    ("toml", "toml"),
    ("yaml", "yaml"),
    ("markdown", "markdown"),
    ("nix", "nix"),
    ("shell", "shell"),
    ("python", "python"),
    ("go", "go"),
    ("html", "html"),
    ("css", "css"),
    ("image", "image"),
    ("lock", "lockFile"),
    ("git", "gitignore"),
    ("c", "c"),
    ("cpp", "cpp"),
    ("header", "h"),
    ("java", "java"),
    ("kotlin", "kotlin"),
    ("ruby", "ruby"),
    ("swift", "swiftLang"),
    ("sql", "sql"),
    ("xml", "xml"),
    ("csv", "csv"),
    ("graphql", "graphql"),
    ("terraform", "terraform"),
    ("docker", "docker"),
    ("config", "config"),
    ("editorconfig", "editorConfig"),
    ("archive", "archive"),
    ("font", "font"),
    ("binary", "binaryData"),
    ("folder", "folder"),
    ("folder_open", "folderOpen"),
    ("folder_github", "folderGithub"),
    ("text", "text"),
    // A licence reads as plain text (IntelliJ's own way); named apart so a cell can say it.
    ("license", "text"),
];

/// The kinds' names, in [`KINDS`]'s order (an error lists them).
pub const KIND_NAMES: [&str; KINDS.len()] = {
    let mut out = [""; KINDS.len()];
    let mut i = 0;
    while i < KINDS.len() {
        out[i] = KINDS[i].0;
        i += 1;
    }
    out
};

/// The drawing kind `kind` is shown with (plain text's for a kind not in [`KINDS`]).
#[must_use]
pub fn drawing(kind: &str) -> &'static str {
    KINDS.iter().find(|(k, _)| *k == kind).map_or("text", |(_, d)| *d)
}

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
        "rb" => "ruby",
        "kt" => "kotlin",
        "h" | "hpp" => "header",
        "package" => "json",
        "picture" | "photo" | "png" | "jpg" | "svg" => "image",
        "dir" | "directory" => "folder",
        "folder-open" => "folder_open",
        "zip" | "tar" => "archive",
        "file" | "generic" | "doc" | "txt" => "text",
        other => other,
    };
    KIND_NAMES.iter().copied().find(|x| *x == k)
}

/// The kind of the place at `path`: a folder's (by its name) when `folder`, else the file's ([`kind_of`]).
#[must_use]
pub fn kind_at(path: &str, folder: bool) -> &'static str {
    if !folder {
        return kind_of(path);
    }
    let base = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path);
    match base {
        ".github" => "folder_github",
        _ => "folder",
    }
}

/// The file kind of the file at `path`, by its name (den's `Kind::of`): a folder for a trailing `/`, the names of their
/// own (a lock file, a Dockerfile, a licence, a dotfile), else its extension; plain text when none says.
#[must_use]
pub fn kind_of(path: &str) -> &'static str {
    if path.ends_with('/') {
        return kind_at(path, true);
    }
    let base = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    match base.as_str() {
        "flake.nix" | "default.nix" | "shell.nix" => return "nix",
        "cargo.lock" | "flake.lock" | "package-lock.json" | "yarn.lock" | "bun.lock" | "bun.lockb" | "pnpm-lock.yaml" | "uv.lock" | "poetry.lock"
        | "gemfile.lock" | "go.sum" => {
            return "lock";
        }
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".gitkeep" | ".ignore" | ".dockerignore" | ".npmignore" | ".prettierignore" => {
            return "git";
        }
        ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".zprofile" | ".zshenv" | ".envrc" => return "shell",
        "dockerfile" | "containerfile" | "docker-compose.yml" | "docker-compose.yaml" | "compose.yml" | "compose.yaml" => return "docker",
        ".editorconfig" => return "editorconfig",
        "makefile" | "gnumakefile" | "justfile" | ".env" | ".npmrc" | ".clang-format" | "rustfmt.toml" | ".rustfmt.toml" => return "config",
        "gemfile" | "rakefile" => return "ruby",
        _ => {}
    }
    // LICENSE, LICENSE-MIT, LICENCE.txt, COPYING, NOTICE.
    if ["license", "licence", "copying", "notice"].iter().any(|l| base.strip_prefix(l).is_some_and(|r| r.is_empty() || r.starts_with(['-', '.']))) {
        return "license";
    }
    if base.starts_with("dockerfile.") || base.ends_with(".dockerfile") {
        return "docker";
    }
    let ext = base.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "json" | "jsonc" | "json5" | "jsonl" | "ndjson" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "mdx" | "markdown" => "markdown",
        "nix" => "nix",
        "sh" | "bash" | "zsh" | "fish" | "nu" => "shell",
        "py" | "pyi" => "python",
        "go" => "go",
        "html" | "htm" => "html",
        "css" | "scss" | "sass" | "less" => "css",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "bmp" | "tiff" | "avif" | "heic" => "image",
        "lock" => "lock",
        "c" => "c",
        "cc" | "cpp" | "cxx" | "c++" | "mm" => "cpp",
        "h" | "hh" | "hpp" | "hxx" => "header",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "rb" => "ruby",
        "swift" => "swift",
        "sql" => "sql",
        "xml" | "plist" | "xsd" | "xsl" => "xml",
        "csv" | "tsv" => "csv",
        "graphql" | "gql" => "graphql",
        "tf" | "tfvars" | "hcl" => "terraform",
        "ini" | "cfg" | "conf" | "properties" | "env" => "config",
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "jar" => "archive",
        "ttf" | "otf" | "woff" | "woff2" => "font",
        "bin" | "o" | "a" | "so" | "dylib" | "dll" | "exe" | "wasm" | "rlib" | "parquet" => "binary",
        _ => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_place_maps_by_its_name_then_its_extension() {
        let of = kind_of;
        assert_eq!((of("crates/a/src/lib.rs"), of("/x/Cargo.toml"), of("flake.nix")), ("rust", "toml", "nix"));
        assert_eq!((of("Cargo.lock"), of("deps.lock"), of("package-lock.json")), ("lock", "lock", "lock"), "a lock file by its name");
        assert_eq!((of(".gitignore"), of("a/.dockerignore"), of("Dockerfile"), of("api.Dockerfile")), ("git", "git", "docker", "docker"));
        assert_eq!((of("LICENSE"), of("LICENSE-MIT"), of("COPYING.txt"), of("licenses.rs")), ("license", "license", "license", "rust"));
        assert_eq!((of("AGENTS.md"), of("README"), of("justfile"), of("a.unknown")), ("markdown", "text", "config", "text"));
        assert_eq!((of("src/"), of(".github/"), of("x.TSX"), of("a.tar.gz")), ("folder", "folder_github", "tsx", "archive"));
        assert_eq!((kind_at("/Volumes/Projects/blaze", true), kind_at("/r/.github", true), kind_at("/r/a.rs", false)), ("folder", "folder_github", "rust"));
    }

    #[test]
    fn every_kind_has_a_drawing_and_a_name() {
        assert_eq!(KIND_NAMES.len(), KINDS.len());
        let mut names = KIND_NAMES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), KINDS.len(), "each kind once");
        assert_eq!((drawing("rust"), drawing("folder"), drawing("tsx"), drawing("no-such")), ("rust", "folder", "typeScript", "text"));
        assert_eq!((kind_named("RS"), kind_named("dir"), kind_named("h"), kind_named("nope")), (Some("rust"), Some("folder"), Some("header"), None));
    }
}
