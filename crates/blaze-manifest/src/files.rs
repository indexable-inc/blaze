//! Every file the manifest, workspace and config readers look at, by absolute path as cargo names
//! it. [`Disk`] reads the machine's; the workspace-load queries (`crates/blaze-units/src/load/`)
//! read a view's through `blaze_query::Cx`. The set is closed and known where each reader is
//! made, so readers take it as a type parameter, never as a trait object.

use std::borrow::Cow;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use anyhow::Context as _;
use cargo_util_schemas::manifest::TomlManifest;

pub trait Files {
    /// The bytes of the file at `path`: borrowed where the files are kept already (a view's,
    /// in the database), owned where they had to be read (the disk).
    fn read(&self, path: &Path) -> std::io::Result<Cow<'_, [u8]>>;
    /// The manifest at `path` parsed, as [`crate::manifest::read_manifest`] reads it, with its
    /// errors: one that can't be read, isn't UTF-8 or isn't TOML of a manifest names `path`.
    /// A `Files` that keeps manifests parsed answers from them, so nothing parses one twice.
    fn manifest(&self, path: &Path) -> anyhow::Result<TomlManifest> {
        // alloc: an error's context, made only when the read fails.
        let bytes = self.read(path).with_context(|| format!("failed to read `{}`", path.display()))?;
        crate::manifest::parse_manifest(path, &bytes)
    }
    /// What is at `path`, following links; `None` for nothing there.
    fn kind(&self, path: &Path) -> Option<FileKind>;
    /// The names in a directory, sorted, each with whether it is a directory (a link to one is
    /// not, as cargo's target discovery reads `DirEntry::file_type`). Names that are not UTF-8
    /// are left out: no reader here matches them.
    fn list(&self, path: &Path) -> std::io::Result<Vec<(String, bool)>>;
    /// The path with every link resolved, as `std::fs::canonicalize`; an error when nothing is
    /// there.
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf>;

    fn exists(&self, path: &Path) -> bool {
        self.kind(path).is_some()
    }

    fn is_file(&self, path: &Path) -> bool {
        self.kind(path) == Some(FileKind::File)
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.kind(path) == Some(FileKind::Dir)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    /// Neither (a socket, a device): it exists, but is no file to read.
    Other,
}

/// The machine's file system, as `std::fs` sees it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Disk;

impl Files for Disk {
    fn read(&self, path: &Path) -> std::io::Result<Cow<'_, [u8]>> {
        // alloc: the file's bytes, read from the disk
        std::fs::read(path).map(Cow::Owned)
    }

    fn kind(&self, path: &Path) -> Option<FileKind> {
        let meta = std::fs::metadata(path).ok()?;
        Some(if meta.is_dir() {
            FileKind::Dir
        } else if meta.is_file() {
            FileKind::File
        } else {
            FileKind::Other
        })
    }

    fn list(&self, path: &Path) -> std::io::Result<Vec<(String, bool)>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else { continue };
            names.push((name, entry.file_type().is_ok_and(|t| t.is_dir())));
        }
        names.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(names)
    }

    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        path.canonicalize()
    }
}

/// A directory of a glob pattern that could not be listed: `glob::GlobError`'s text, which
/// cargo's "unable to match path to pattern" context wraps.
#[derive(Debug)]
pub struct GlobError {
    path: PathBuf,
    error: std::io::Error,
}

impl fmt::Display for GlobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "attempting to read `{}` resulted in an error: {}",
            self.path.display(),
            self.error
        )
    }
}

impl std::error::Error for GlobError {
    // dyn: std's error chain, read only on an error path.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// One component of a pattern, parsed once.
struct Part {
    pattern: glob::Pattern,
    /// `glob::Pattern`'s `has_metachars`: a `*`, `?` or `[` (a `[` that opens no class is a
    /// pattern error already).
    meta: bool,
    /// `**`, which `glob::Pattern::new` only accepts as a whole component.
    recursive: bool,
    /// The first token is a literal `.`, so `.` and `..` match.
    dot: bool,
}

/// A path to match against the parts from `.1` on; `usize::MAX` for one already matched whole.
struct Todo {
    path: PathBuf,
    dir: bool,
    at: usize,
}

/// `glob::glob(pattern)` read through `files`: the same matches in the same (alphabetical) order,
/// the same pattern errors, and a [`GlobError`] where glob would give one. Ported from glob
/// 0.3.3's `glob_with` and `fill_todo` with the default `MatchOptions`.
pub fn glob<F: Files>(
    files: &F,
    pattern: &str,
) -> Result<Vec<Result<PathBuf, GlobError>>, glob::PatternError> {
    glob::Pattern::new(pattern)?;
    let mut components = Path::new(pattern).components().peekable();
    while matches!(components.peek(), Some(Component::Prefix(..) | Component::RootDir)) {
        components.next();
    }
    let rest: PathBuf = components.map(|c| c.as_os_str()).collect();
    let normalized: PathBuf = Path::new(pattern).iter().collect();
    let root_len = normalized.as_os_str().len() - rest.as_os_str().len();
    let scope = if root_len > 0 { PathBuf::from(&pattern[..root_len]) } else { PathBuf::from(".") };

    let mut parts = Vec::new();
    for component in pattern[root_len.min(pattern.len())..].split_terminator(std::path::is_separator)
    {
        parts.push(Part {
            pattern: glob::Pattern::new(component)?,
            meta: component.contains(['*', '?', '[']),
            recursive: component == "**",
            dot: component.starts_with('.'),
        });
    }
    if root_len == pattern.len() {
        parts.push(Part {
            pattern: glob::Pattern::new("")?,
            meta: false,
            recursive: false,
            dot: false,
        });
    }
    let require_dir = pattern.chars().next_back().is_some_and(std::path::is_separator);
    let options = glob::MatchOptions::new();

    let mut out = Vec::new();
    let mut todo: Vec<Result<Todo, GlobError>> = Vec::new();
    let scope_dir = files.is_dir(&scope);
    fill(files, &mut todo, &parts, 0, &scope, scope_dir);
    while let Some(next) = todo.pop() {
        let Todo { path, dir, mut at } = match next {
            Ok(next) => next,
            Err(e) => {
                out.push(Err(e));
                continue;
            }
        };
        if at == usize::MAX {
            if !require_dir || dir {
                out.push(Ok(path));
            }
            continue;
        }
        if parts[at].recursive {
            let mut last = at;
            while last + 1 < parts.len() && parts[last + 1].recursive {
                last += 1;
            }
            if dir {
                fill(files, &mut todo, &parts, last, &path, dir);
                if last == parts.len() - 1 {
                    out.push(Ok(path));
                    continue;
                }
                at = last + 1;
            } else if last == parts.len() - 1 {
                continue;
            } else {
                at = last + 1;
            }
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
        if parts[at].pattern.matches_with(name, options) {
            if at == parts.len() - 1 {
                if !require_dir || dir {
                    out.push(Ok(path));
                }
            } else {
                fill(files, &mut todo, &parts, at + 1, &path, dir);
            }
        }
    }
    Ok(out)
}

fn fill<F: Files>(
    files: &F,
    todo: &mut Vec<Result<Todo, GlobError>>,
    parts: &[Part],
    at: usize,
    path: &Path,
    dir: bool,
) {
    let add = |todo: &mut Vec<Result<Todo, GlobError>>, next: PathBuf| {
        let next_dir = files.is_dir(&next);
        if at + 1 == parts.len() {
            todo.push(Ok(Todo { path: next, dir: next_dir, at: usize::MAX }));
        } else {
            fill(files, todo, parts, at + 1, &next, next_dir);
        }
    };
    let part = &parts[at];
    let curdir = path == Path::new(".");
    if !part.meta {
        let s = part.pattern.as_str();
        let special = s == "." || s == "..";
        let next = if curdir { PathBuf::from(s) } else { path.join(s) };
        if (special && dir) || (!special && files.exists(&next)) {
            add(todo, next);
        }
    } else if dir {
        match files.list(path) {
            Ok(names) => {
                for (name, is_dir) in names.into_iter().rev() {
                    let child = if curdir { PathBuf::from(&name) } else { path.join(&name) };
                    // glob follows a link to learn whether it is a directory.
                    let child_dir = is_dir || files.is_dir(&child);
                    todo.push(Ok(Todo { path: child, dir: child_dir, at }));
                }
                if part.dot {
                    for special in [".", ".."] {
                        if part.pattern.matches_with(special, glob::MatchOptions::new()) {
                            add(todo, path.join(special));
                        }
                    }
                }
            }
            Err(error) => todo.push(Err(GlobError { path: path.to_path_buf(), error })),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A tree in memory: each path a file (`Some` bytes) or a directory (`None`).
    #[derive(Default)]
    pub(crate) struct Mem(pub BTreeMap<PathBuf, Option<Vec<u8>>>);

    impl Mem {
        pub(crate) fn file(mut self, path: &str, text: &str) -> Mem {
            let path = PathBuf::from(path);
            for dir in path.ancestors().skip(1) {
                self.0.entry(dir.to_path_buf()).or_insert(None);
            }
            self.0.insert(path, Some(text.as_bytes().to_vec()));
            self
        }

        pub(crate) fn dir(mut self, path: &str) -> Mem {
            for dir in Path::new(path).ancestors() {
                self.0.entry(dir.to_path_buf()).or_insert(None);
            }
            self
        }
    }

    fn not_found() -> std::io::Error {
        std::io::Error::from(std::io::ErrorKind::NotFound)
    }

    impl Files for Mem {
        fn read(&self, path: &Path) -> std::io::Result<Cow<'_, [u8]>> {
            match self.0.get(&crate::paths::normalize_path(path)) {
                Some(Some(bytes)) => Ok(Cow::Borrowed(bytes)),
                Some(None) => Err(std::io::Error::other("is a directory")),
                None => Err(not_found()),
            }
        }

        fn kind(&self, path: &Path) -> Option<FileKind> {
            let path = crate::paths::normalize_path(path);
            self.0.get(&path).map(|it| if it.is_some() { FileKind::File } else { FileKind::Dir })
        }

        fn list(&self, path: &Path) -> std::io::Result<Vec<(String, bool)>> {
            if self.kind(path) != Some(FileKind::Dir) {
                return Err(not_found());
            }
            let path = crate::paths::normalize_path(path);
            Ok(self
                .0
                .iter()
                .filter(|(p, _)| p.parent() == Some(path.as_path()))
                .map(|(p, it)| (p.file_name().unwrap().to_str().unwrap().to_owned(), it.is_none()))
                .collect())
        }

        fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
            let path = crate::paths::normalize_path(path);
            if self.0.contains_key(&path) { Ok(path) } else { Err(not_found()) }
        }
    }

    fn matched(files: &impl Files, pattern: &str) -> Vec<String> {
        glob(files, pattern)
            .unwrap()
            .into_iter()
            .map(|it| it.unwrap().display().to_string())
            .collect()
    }

    fn tree() -> Mem {
        Mem::default()
            .file("/w/crates/b/Cargo.toml", "")
            .file("/w/crates/a/Cargo.toml", "")
            .file("/w/crates/.hidden/Cargo.toml", "")
            .file("/w/crates/notes.md", "")
            .dir("/w/crates/a/src/x")
            .file("/w/tools/t1/Cargo.toml", "")
    }

    #[test]
    fn a_star_matches_every_name_in_order_dotfiles_too() {
        assert_eq!(
            matched(&tree(), "/w/crates/*"),
            ["/w/crates/.hidden", "/w/crates/a", "/w/crates/b", "/w/crates/notes.md"]
        );
    }

    #[test]
    fn a_trailing_separator_keeps_directories_only() {
        assert_eq!(matched(&tree(), "/w/crates/*/"), ["/w/crates/.hidden", "/w/crates/a", "/w/crates/b"]);
    }

    #[test]
    fn a_literal_path_matches_when_it_is_there() {
        assert_eq!(matched(&tree(), "/w/tools/t1"), ["/w/tools/t1"]);
        assert!(matched(&tree(), "/w/tools/t2").is_empty());
    }

    #[test]
    fn classes_and_question_marks_match_one_character() {
        assert_eq!(matched(&tree(), "/w/crates/[ab]"), ["/w/crates/a", "/w/crates/b"]);
        assert_eq!(matched(&tree(), "/w/crates/[!a]"), ["/w/crates/b"]);
        assert_eq!(matched(&tree(), "/w/?ools/*"), ["/w/tools/t1"]);
    }

    #[test]
    fn a_recursive_part_matches_every_depth() {
        // A trailing `**` is glob's: the directories under the base, neither it nor files.
        assert_eq!(matched(&tree(), "/w/crates/a/**"), ["/w/crates/a/src", "/w/crates/a/src/x"]);
        assert_eq!(
            matched(&tree(), "/w/**/Cargo.toml"),
            [
                "/w/crates/.hidden/Cargo.toml",
                "/w/crates/a/Cargo.toml",
                "/w/crates/b/Cargo.toml",
                "/w/tools/t1/Cargo.toml",
            ]
        );
    }

    /// Files kept in memory are lent, not copied, and a manifest is parsed from them with the
    /// reader's errors.
    #[test]
    fn kept_bytes_are_lent() {
        let mem = Mem::default().file("/w/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\n");
        let kept = mem.0[Path::new("/w/Cargo.toml")].as_ref().unwrap().as_ptr();
        let read = mem.read(Path::new("/w/Cargo.toml")).unwrap();
        assert!(matches!(read, Cow::Borrowed(it) if it.as_ptr() == kept));
        assert!(mem.manifest(Path::new("/w/Cargo.toml")).unwrap().package().is_some());
        let absent = mem.manifest(Path::new("/w/b/Cargo.toml")).unwrap_err().to_string();
        assert_eq!(absent, "failed to read `/w/b/Cargo.toml`");
        let bad = Mem::default().file("/w/Cargo.toml", "[package");
        assert!(bad.manifest(Path::new("/w/Cargo.toml")).is_err());
    }

    #[test]
    fn a_bad_pattern_is_glob_s_error() {
        assert!(glob(&tree(), "/w/crates/[").is_err());
        assert!(glob(&tree(), "/w/***").is_err());
    }

    #[test]
    fn a_directory_that_cannot_be_listed_is_an_error_naming_it() {
        struct Unlistable;
        impl Files for Unlistable {
            fn read(&self, _: &Path) -> std::io::Result<Cow<'_, [u8]>> {
                Err(not_found())
            }
            fn kind(&self, _: &Path) -> Option<FileKind> {
                Some(FileKind::Dir)
            }
            fn list(&self, _: &Path) -> std::io::Result<Vec<(String, bool)>> {
                Err(std::io::Error::other("denied"))
            }
            fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
                Ok(path.to_path_buf())
            }
        }
        let got = glob(&Unlistable, "/w/*").unwrap();
        let [Err(e)] = &got[..] else { panic!("{got:?}") };
        assert_eq!(e.to_string(), "attempting to read `/w` resulted in an error: denied");
    }

    /// A workspace with glob members, inherited fields, a `.cargo/config.toml` and discovered
    /// targets, as `(path, text)` under `root`.
    fn workspace_files(root: &str) -> Vec<(String, &'static str)> {
        [
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n\
                 [workspace.package]\nversion = \"1.2.3\"\nedition = \"2021\"\n\
                 [workspace.dependencies]\nb = { path = \"crates/b\" }\n",
            ),
            (".cargo/config.toml", "[build]\ntarget-dir = \"out\"\n"),
            (
                "crates/a/Cargo.toml",
                "[package]\nname = \"a\"\nversion.workspace = true\nedition.workspace = true\n\
                 [dependencies]\nb.workspace = true\n",
            ),
            ("crates/a/src/lib.rs", ""),
            ("crates/a/src/bin/tool.rs", ""),
            ("crates/a/src/bin/multi/main.rs", ""),
            ("crates/a/examples/demo.rs", ""),
            ("crates/a/build.rs", ""),
            ("crates/a/README.md", ""),
            ("crates/b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
            ("crates/b/src/main.rs", ""),
            ("crates/b/tests/it.rs", ""),
        ]
        .into_iter()
        .map(|(path, text)| (format!("{root}/{path}"), text))
        .collect()
    }

    fn metadata<F: Files>(root: &str, files: F) -> String {
        let env = [
            ("CARGO_HOME".to_owned(), format!("{root}/home")),
            ("__CARGO_TEST_ROOT".to_owned(), root.to_owned()),
        ];
        let config = crate::Config::load_with(Path::new(root), env, files).unwrap();
        let ctx = crate::Context::new(config);
        let manifest = crate::metadata::root_manifest(None, &ctx).unwrap();
        let ws = crate::Workspace::new(&manifest, &ctx).unwrap();
        serde_json::to_string_pretty(&crate::metadata::export_no_deps(&ws)).unwrap()
    }

    /// The same workspace read through files in memory and through the machine's disk gives the
    /// same `cargo metadata --no-deps`.
    #[test]
    fn a_workspace_reads_the_same_through_any_files() {
        let scratch = std::env::temp_dir().join(format!("blaze-manifest-files-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).unwrap();
        let root = scratch.canonicalize().unwrap().display().to_string();
        let mut mem = Mem::default().dir(&format!("{root}/home"));
        for (path, text) in workspace_files(&root) {
            std::fs::create_dir_all(Path::new(&path).parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            mem = mem.file(&path, text);
        }
        std::fs::create_dir_all(format!("{root}/home")).unwrap();
        let on_disk = metadata(&root, Disk);
        std::fs::remove_dir_all(&scratch).unwrap();
        let in_memory = metadata(&root, mem);
        assert_eq!(in_memory, on_disk);
        for name in ["\"tool\"", "\"multi\"", "\"demo\"", "\"build-script-build\"", "\"it\""] {
            assert!(on_disk.contains(name), "{name} in {on_disk}");
        }
        assert!(on_disk.contains(&format!("{root}/out")), "{on_disk}");
        assert!(on_disk.contains("\"1.2.3\""), "{on_disk}");
    }

    /// A workspace that exists only in the files given is read without the disk.
    #[test]
    fn a_workspace_only_the_files_hold_reads() {
        let root = "/blaze-nowhere/w";
        let mut mem = Mem::default().dir(&format!("{root}/home"));
        for (path, text) in workspace_files(root) {
            mem = mem.file(&path, text);
        }
        let json = metadata(root, mem);
        assert!(json.contains("\"/blaze-nowhere/w/out\""), "{json}");
        assert!(json.contains("/blaze-nowhere/w/crates/a/src/bin/multi/main.rs"), "{json}");
    }

    /// On the machine's disk the port gives what glob itself gives.
    #[test]
    fn on_disk_it_is_glob() {
        let root = std::env::temp_dir().join(format!("blaze-manifest-glob-{}", std::process::id()));
        for dir in ["m/a/src", "m/b", "m/.c", "n"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("m/a/Cargo.toml"), "").unwrap();
        std::fs::write(root.join("m/file"), "").unwrap();
        let root = root.canonicalize().unwrap();
        for pattern in ["m/*", "m/*/", "m/**", "m/**/Cargo.toml", "[mn]", "m/a", "m/zz", "m/.*"] {
            let pattern = format!("{}/{pattern}", root.display());
            let ours: Vec<PathBuf> = glob(&Disk, &pattern).unwrap().into_iter().map(Result::unwrap).collect();
            let theirs: Vec<PathBuf> = glob::glob(&pattern).unwrap().map(Result::unwrap).collect();
            assert_eq!(ours, theirs, "{pattern}");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
}
