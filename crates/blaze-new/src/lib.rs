//! `cargo new` and `cargo init`, made by blaze with no cargo: `src/ops/cargo_new.rs` and
//! `src/util/vcs.rs` of cargo 1.101.0 (cad07cf8e5). For a package of a Rust edition it writes
//! what cargo writes, byte for byte: the manifest, the source file, the ignore file, the
//! repository, and the workspace's `members`. For a package of a blaze edition
//! (docs/v2/plan/design/std.md, "The blaze edition: opting in") the manifest says
//! `edition = "blaze-2026.11"` and the source is on bstd: a `main` that takes `&Cx` and the
//! one grant it uses, `out: Out` (D104), and writes its line through it. Which it writes when no edition is asked is
//! [`default_edition`]. cargo runs rustfmt on the source file it writes, which is formatted already;
//! blaze runs no rustfmt. A git repository is made by `git init`, where cargo has libgit2.
//!
//! The manifest is written by blaze-toml's writers in the layout cargo's toml_edit gives a new
//! document; the root manifest's `members` gets the new package by a [`blaze_toml::Edit`],
//! which keeps every other byte and follows the array's own layout (where cargo's toml_edit,
//! pushing and then sorting, can leave a sorted one-line array's spaces out of place).

#![feature(allocator_api)]

use std::alloc::Global;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use blaze_data::Value;
use blaze_manifest::error::ManifestError;
use blaze_manifest::schema::PackageName;
use blaze_toml::{Document, EditError, Item, Limits, Origin, write_string};

/// The edition `cargo new` writes when none is asked.
const LATEST_STABLE: &str = "2024";

/// Whether this blaze ships bstd with itself and gives it to every crate of a blaze edition
/// (docs/v2/plan/design/std.md, row S5d). Not yet: a package of the edition names bstd as a
/// dependency, so `new` can write one only where it is told where bstd is
/// ([`NewOptions::bstd`]). Turning this to `true` makes the blaze edition what `new` and
/// `init` write when no edition is asked, with no `bstd` line.
pub const BSTD_SHIPS: bool = false;

/// The digest of the bstd source tree the release this blaze is ships beside it
/// (`bstd-<digest>.tar.xz`: crates/blaze-std, the crates of blaze it depends on by path and a
/// workspace manifest of just them, digested as the library is), when it ships one: given to
/// the build of a release as `BLAZE_BSTD_DIGEST` by the step that made the archive
/// (harness/library.py `archive` with `"tree": "bstd"`, in release.yaml's notes job), none
/// for a build of the tree. A shipped bstd is placed at
/// `<machine>/bstd/<digest>/bstd` and its records are a sealed layer as the library's are
/// (`cli::store::shipped` of blaze-cargo, the one list of what ships); a package names it there
/// with `--bstd`. A bstd that is a path into a checkout is no shipped tree: its sources can
/// change, so it is built in its home like any path crate.
pub const BSTD_DIGEST: Option<&str> = match option_env!("BLAZE_BSTD_DIGEST") {
    Some(digest) if !digest.is_empty() => Some(digest),
    _ => None,
};

/// The edition written when none is asked: the latest blaze edition where a package of it
/// can be built (bstd ships, or `bstd` says where it is), else Rust's latest. `--rust` on the
/// command line asks for Rust's by name, whatever this is.
pub fn default_edition(bstd: bool) -> &'static str {
    match BSTD_SHIPS || bstd {
        true => edition::BlazeEdition::LATEST.manifest(),
        false => LATEST_STABLE,
    }
}

/// The `main.rs` of a package of a blaze edition: each grant it uses named as a parameter
/// (D104: here only standard output, `out: Out`), its line through it, the write's error
/// returned to the entry.
const BLAZE_MAIN: &str = "\
fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    out.line(cx, \"Hello, world!\")?;
    Ok(())
}
";

/// The `lib.rs` of a package of a blaze edition. No test module: a test of the edition takes
/// the simulation's `Io`, which is not built yet (std.md, row S5b).
const BLAZE_LIB: &str = "\
pub fn add(left: u64, right: u64) -> u64 {
    left + right
}
";

/// What a command says on its way, as cargo's shell says it.
pub trait Shell {
    /// `    Creating binary (application) `x` package`.
    fn status(&mut self, word: &str, rest: &str);
    fn warn(&mut self, text: &str);
    fn note(&mut self, text: &str);
    /// A line of standard error as it is.
    fn line(&mut self, text: &str);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionControl {
    Git,
    Hg,
    Pijul,
    Fossil,
    NoVcs,
}

impl VersionControl {
    pub const VALUES: [&str; 5] = ["git", "hg", "pijul", "fossil", "none"];

    pub fn parse(name: &str) -> Option<VersionControl> {
        Some(match name {
            "git" => VersionControl::Git,
            "hg" => VersionControl::Hg,
            "pijul" => VersionControl::Pijul,
            "fossil" => VersionControl::Fossil,
            "none" => VersionControl::NoVcs,
            _ => return None,
        })
    }
}

#[derive(Debug)]
pub struct NewOptions {
    pub version_control: Option<VersionControl>,
    pub kind: NewProjectKind,
    pub auto_detect_kind: bool,
    /// Absolute path to the directory for the new package.
    pub path: PathBuf,
    pub name: Option<String>,
    pub edition: Option<String>,
    pub registry: Option<String>,
    /// Where bstd's package (crates/blaze-std) is, as the manifest's `bstd` dependency will
    /// say it: needed by a package of a blaze edition until blaze ships bstd
    /// ([`BSTD_SHIPS`]), and what makes that edition the default.
    pub bstd: Option<String>,
}

/// What cargo's configuration and environment say that `new` and `init` read.
pub struct Settings<'a> {
    pub cwd: &'a Path,
    pub home: Option<&'a Path>,
    /// `cargo-new.vcs`.
    pub vcs: Option<VersionControl>,
    /// Why the new manifest's workspace does not read, said as cargo says it: blaze-manifest's
    /// `Workspace::new` of it.
    // dyn: asked once per `new` or `init`.
    pub check_workspace: &'a dyn Fn(&Path) -> Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewProjectKind {
    Bin,
    Lib,
}

impl NewProjectKind {
    fn is_bin(self) -> bool {
        self == NewProjectKind::Bin
    }
}

impl fmt::Display for NewProjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            NewProjectKind::Bin => "binary (application)",
            NewProjectKind::Lib => "library",
        }
        .fmt(f)
    }
}

struct SourceFileInformation {
    relative_path: String,
    bin: bool,
}

struct MkOptions<'a> {
    version_control: Option<VersionControl>,
    path: &'a Path,
    name: &'a str,
    source_files: Vec<SourceFileInformation>,
    edition: Option<&'a str>,
    registry: Option<&'a str>,
    bstd: Option<&'a str>,
}

/// What a file operation was doing, in cargo's words for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doing {
    /// `failed to read `{path}``.
    Read,
    /// `failed to open: {path}`, an ignore file.
    Open,
    /// `failed to write: {path}`, an ignore file.
    Append,
    /// `failed to write `{path}``.
    Write,
    /// `failed to create directory `{path}``.
    CreateDir,
}

/// Why `new` or `init` refused, or failed, in cargo's words; the cause, where there is one, is
/// its [`source`](core::error::Error::source), as cargo's `Caused by:` shows it.
#[derive(Debug)]
pub enum NewError {
    /// `--bin` and `--lib` both.
    BothKinds,
    /// No directory name to take the package's name from.
    NoName { path: PathBuf },
    NonUnicodeName { name: OsString },
    /// A name that is no package name: blaze-manifest's refusal, then the help.
    InvalidName { error: ManifestError, help: String },
    Keyword { name: String, help: String },
    /// `deps`, `examples`, `build` or `incremental`, for a package with a binary.
    ArtifactName { name: String, help: String },
    /// `test`.
    TestName { help: String },
    WindowsReserved { name: String, help: String },
    /// Two binaries `init` found, either of which could be the main one.
    TwoBinaries { first: String, second: String },
    TwoLibraries { first: String, second: String },
    /// `new` of a path that is there.
    Exists { path: PathBuf },
    /// `init` of the home directory.
    InHome,
    /// `init` where a Cargo.toml is.
    IsPackage,
    /// `init` with more than one repository's directory there and no `--vcs`.
    ManyVcs,
    /// A line of an ignore file that is not UTF-8: its index from 0, as cargo counts it.
    IgnoreNotUtf8 { line: usize },
    Io { doing: Doing, path: PathBuf, error: std::io::Error },
    /// A version control program that did not start.
    Spawn { program: &'static str, error: std::io::Error },
    /// One that started and failed.
    Exited { program: &'static str, status: ExitStatus },
    /// `git init` failed: its standard error.
    GitInit { path: PathBuf, stderr: String },
    /// A blaze edition asked for with no bstd to give it.
    NeedsBstd { edition: String },
    /// An entry of the root manifest's `members`, or of its `exclude` when `exclude`, that is
    /// not a string, as written.
    NotAString { exclude: bool, written: String },
    Pattern { pattern: String, error: blaze_glob::PatternError },
    /// The root manifest's `members` could not be edited.
    Edit { path: PathBuf, error: EditError<Global> },
    NoParent { path: PathBuf },
    /// The package's path and the workspace's are not both absolute.
    NotRelative { package: PathBuf, workspace: PathBuf },
    NonUnicodePath { path: PathBuf },
    /// What making the package was refused or failed by.
    Create { name: String, path: PathBuf, source: Box<NewError> },
}

impl fmt::Display for NewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NewError::BothKinds => f.write_str("can't specify both lib and binary outputs"),
            NewError::NoName { path } => {
                write!(f, "cannot auto-detect package name from path {:?} ; use --name to override", path.as_os_str())
            }
            NewError::NonUnicodeName { name } => write!(f, "cannot create package with a non-unicode name: {name:?}"),
            NewError::InvalidName { error, help } => write!(f, "{error}{help}"),
            NewError::Keyword { name, help } => write!(f, "invalid package name `{name}`: it is a Rust keyword{help}"),
            NewError::ArtifactName { name, help } => {
                write!(f, "invalid package name `{name}`: it conflicts with cargo's build directory names{help}")
            }
            NewError::TestName { help } => {
                write!(f, "invalid package name `test`: it conflicts with Rust's built-in test library{help}")
            }
            NewError::WindowsReserved { name, help } => {
                write!(f, "invalid package name `{name}`: it is a reserved Windows filename{help}")
            }
            NewError::TwoBinaries { first, second } => write!(
                f,
                "multiple possible binary sources found:\n  {first}\n  {second}\n\
                 cannot automatically generate Cargo.toml as the main target would be ambiguous"
            ),
            NewError::TwoLibraries { first, second } => {
                write!(f, "cannot have a package with multiple libraries, found both `{first}` and `{second}`")
            }
            NewError::Exists { path } => {
                write!(f, "destination `{}` already exists\n\nUse `cargo init` to initialize the directory", path.display())
            }
            NewError::InHome => f.write_str(
                "cannot create package in the home directory\n\n\
                 help: use `cargo init <path>` to create a package in a different directory",
            ),
            NewError::IsPackage => f.write_str(
                "`cargo init` cannot be run on existing Cargo packages\n\
                 help: use `cargo new` to create a package in a new subdirectory",
            ),
            NewError::ManyVcs => f.write_str(
                "more than one of .hg, .git, .pijul, .fossil configurations found and the ignore file can't be filled in as a result. specify --vcs to override detection",
            ),
            NewError::IgnoreNotUtf8 { line } => write!(f, "Character at line {line} is invalid. Cargo only supports UTF-8."),
            NewError::Io { doing, path, .. } => match doing {
                Doing::Read => write!(f, "failed to read `{}`", path.display()),
                Doing::Open => write!(f, "failed to open: {}", path.display()),
                Doing::Append => write!(f, "failed to write: {}", path.display()),
                Doing::Write => write!(f, "failed to write `{}`", path.display()),
                Doing::CreateDir => write!(f, "failed to create directory `{}`", path.display()),
            },
            NewError::Spawn { program, .. } => write!(f, "could not execute process `{program}` (never executed)"),
            NewError::Exited { program, status } => write!(f, "process didn't exit successfully: `{program}` ({status})"),
            NewError::GitInit { path, stderr } => write!(f, "failed to make a git repository at `{}`: {stderr}", path.display()),
            NewError::NeedsBstd { edition } => write!(
                f,
                "a package of the blaze edition `{edition}` needs bstd, which blaze does not ship with \
                 itself yet (docs/v2/plan/design/std.md, row S5d): say where its package is with \
                 `--bstd <PATH>` (crates/blaze-std of blaze's source)"
            ),
            NewError::NotAString { exclude: true, written } => write!(f, "invalid non-string exclude path `{written}`"),
            NewError::NotAString { exclude: false, written } => write!(f, "invalid non-string member `{written}`"),
            NewError::Pattern { pattern, .. } => write!(f, "cannot build glob pattern from `{pattern}`"),
            NewError::Edit { path, error } => write!(f, "failed to add the package to `members` in `{}`: {error}", path.display()),
            NewError::NoParent { path } => {
                write!(f, "workspace root manifest doesn't have a parent directory `{}`", path.display())
            }
            NewError::NotRelative { package, workspace } => write!(
                f,
                "path comparison requires two absolute paths; package_path: `{}`, workspace_path: `{}`",
                package.display(),
                workspace.display()
            ),
            NewError::NonUnicodePath { path } => write!(f, "invalid unicode component in path `{}`", path.display()),
            NewError::Create { name, path, .. } => write!(f, "failed to create package `{name}` at `{}`", path.display()),
        }
    }
}

impl core::error::Error for NewError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            NewError::Io { error, .. } | NewError::Spawn { error, .. } => Some(error),
            NewError::Pattern { error, .. } => Some(error),
            NewError::Create { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

fn io_error(doing: Doing, path: &Path) -> impl FnOnce(std::io::Error) -> NewError + '_ {
    move |error| NewError::Io { doing, path: path.to_path_buf(), error }
}

impl NewOptions {
    pub fn new(
        version_control: Option<VersionControl>,
        bin: bool,
        lib: bool,
        path: PathBuf,
        name: Option<String>,
        edition: Option<String>,
        registry: Option<String>,
    ) -> Result<NewOptions, NewError> {
        let auto_detect_kind = !bin && !lib;
        let kind = match (bin, lib) {
            (true, true) => return Err(NewError::BothKinds),
            (false, true) => NewProjectKind::Lib,
            (_, false) => NewProjectKind::Bin,
        };
        Ok(NewOptions {
            version_control,
            kind,
            auto_detect_kind,
            path,
            name,
            edition,
            registry,
            bstd: None,
        })
    }
}

fn get_name<'a>(path: &'a Path, opts: &'a NewOptions) -> Result<&'a str, NewError> {
    if let Some(ref name) = opts.name {
        return Ok(name);
    }
    let file_name = path.file_name().ok_or_else(|| NewError::NoName { path: path.to_path_buf() })?;
    file_name.to_str().ok_or_else(|| NewError::NonUnicodeName { name: file_name.to_owned() })
}

fn is_keyword(name: &str) -> bool {
    [
        "Self", "abstract", "as", "async", "await", "become", "box", "break", "const", "continue",
        "crate", "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "if",
        "impl", "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv",
        "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true", "try",
        "type", "typeof", "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
    ]
    .contains(&name)
}

fn is_windows_reserved(name: &str) -> bool {
    [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ]
    .contains(&name.to_ascii_lowercase().as_str())
}

fn is_conflicting_artifact_name(name: &str) -> bool {
    ["deps", "examples", "build", "incremental"].contains(&name)
}

fn check_name(
    name: &str,
    show_name_help: bool,
    has_bin: bool,
    shell: &mut impl Shell,
) -> Result<(), NewError> {
    // If --name is already used to override, no point in suggesting it again as a fix.
    let name_help = if show_name_help {
        "\nnote: the directory name is used as the package name\
        \nhelp: to override the package name, pass `--name <pkgname>`"
    } else {
        ""
    };
    let bin_help = || {
        let mut help = String::from(name_help);
        // Only suggest `bin.name` for valid crate names because it is used for `--crate`.
        if has_bin && validate_crate_name(name) {
            help.push_str(&format!(
                "\n\
                help: to name the binary \"{name}\", use a valid package \
                name, and set the binary name to be different from the package. \
                This can be done by setting the binary filename to `src/bin/{name}.rs` \
                or change the name in Cargo.toml with:\n\
                \n    \
                [[bin]]\n    \
                name = \"{name}\"\n    \
                path = \"src/main.rs\"\n\
            "
            ));
        }
        help
    };
    PackageName::new(name)
        .map_err(|err| NewError::InvalidName { error: ManifestError::Resolve(err), help: bin_help() })?;

    if is_keyword(name) {
        return Err(NewError::Keyword { name: name.to_owned(), help: bin_help() });
    }
    if is_conflicting_artifact_name(name) {
        if has_bin {
            return Err(NewError::ArtifactName { name: name.to_owned(), help: name_help.to_owned() });
        }
        shell.warn(&format!(
            "package `{name}` will not support binary executables with that name, it conflicts with cargo's build directory names"
        ));
    }
    if name == "test" {
        return Err(NewError::TestName { help: bin_help() });
    }
    if ["core", "std", "alloc", "proc_macro", "proc-macro"].contains(&name) {
        shell.warn(&format!(
            "package name `{name}` may be confused with the package with that name in Rust's standard library\n\
            It is recommended to use a different name to avoid problems.{}",
            bin_help()
        ));
    }
    if is_windows_reserved(name) {
        if cfg!(windows) {
            return Err(NewError::WindowsReserved { name: name.to_owned(), help: name_help.to_owned() });
        }
        shell.warn(&format!(
            "package name `{name}` is a reserved Windows filename\n\
            This package will not work on Windows platforms."
        ));
    }
    if name.chars().any(|ch| ch > '\x7f') {
        shell.warn(&format!(
            "invalid package name `{name}`: contains non-ASCII characters\n\
            Non-ASCII crate names are not supported by Rust."
        ));
    }
    let name_in_lowercase = name.to_lowercase();
    if name != name_in_lowercase {
        shell.warn(&format!(
            "package name `{name}` is not snake_case or kebab-case which is recommended for package names, consider `{name_in_lowercase}`"
        ));
    }
    Ok(())
}

/// rustc's `validate_crate_name`.
fn validate_crate_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

/// Warns if the path has characters that will break `env::join_paths`.
fn check_path(path: &Path, shell: &mut impl Shell) {
    if std::env::join_paths([path]).is_err() {
        let path = path.to_string_lossy();
        shell.warn(&format!(
            "the path `{path}` contains invalid PATH characters (usually `:`, `;`, or `\"`)\n\
            It is recommended to use a different name to avoid problems."
        ));
    }
}

fn detect_source_paths_and_types(
    path: &Path,
    name: &str,
    detected_files: &mut Vec<SourceFileInformation>,
) -> Result<(), NewError> {
    enum H {
        Bin,
        Lib,
        Detect,
    }
    let tests = [
        ("src/main.rs".to_owned(), H::Bin),
        ("main.rs".to_owned(), H::Bin),
        (format!("src/{name}.rs"), H::Detect),
        (format!("{name}.rs"), H::Detect),
        ("src/lib.rs".to_owned(), H::Lib),
        ("lib.rs".to_owned(), H::Lib),
    ];
    for (pp, handling) in tests {
        if !path.join(&pp).is_file() {
            continue;
        }
        let bin = match handling {
            H::Bin => true,
            H::Lib => false,
            H::Detect => {
                let file = path.join(&pp);
                std::fs::read_to_string(&file).map_err(io_error(Doing::Read, &file))?.contains("fn main")
            }
        };
        detected_files.push(SourceFileInformation { relative_path: pp, bin });
    }

    // Check for duplicate lib attempt.
    let mut previous_lib_relpath: Option<&str> = None;
    let mut duplicates_checker: BTreeMap<&str, &SourceFileInformation> = BTreeMap::new();
    for i in detected_files.iter() {
        if i.bin {
            if let Some(x) = duplicates_checker.get(name) {
                return Err(NewError::TwoBinaries {
                    first: x.relative_path.clone(),
                    second: i.relative_path.clone(),
                });
            }
            duplicates_checker.insert(name, i);
        } else {
            if let Some(plp) = previous_lib_relpath {
                return Err(NewError::TwoLibraries { first: plp.to_owned(), second: i.relative_path.clone() });
            }
            previous_lib_relpath = Some(&i.relative_path);
        }
    }
    Ok(())
}

fn plan_new_source_file(bin: bool) -> SourceFileInformation {
    let relative_path = if bin { "src/main.rs" } else { "src/lib.rs" };
    SourceFileInformation { relative_path: relative_path.to_owned(), bin }
}

fn calculate_new_project_kind(
    requested_kind: NewProjectKind,
    auto_detect_kind: bool,
    found_files: &[SourceFileInformation],
) -> NewProjectKind {
    let bin_file = found_files.iter().find(|x| x.bin);
    let kind_from_files = if !found_files.is_empty() && bin_file.is_none() {
        NewProjectKind::Lib
    } else {
        NewProjectKind::Bin
    };
    if auto_detect_kind {
        return kind_from_files;
    }
    requested_kind
}

pub fn new(
    opts: &NewOptions,
    settings: &Settings<'_>,
    shell: &mut impl Shell,
) -> Result<(), NewError> {
    let path = &opts.path;
    let name = get_name(path, opts)?;
    shell.status("Creating", &format!("{} `{name}` package", opts.kind));

    if path.exists() {
        return Err(NewError::Exists { path: path.clone() });
    }
    check_path(path, shell);
    let is_bin = opts.kind.is_bin();
    check_name(name, opts.name.is_none(), is_bin, shell)?;

    let mkopts = MkOptions {
        version_control: opts.version_control,
        path,
        name,
        source_files: vec![plan_new_source_file(opts.kind.is_bin())],
        edition: opts.edition.as_deref(),
        registry: opts.registry.as_deref(),
        bstd: opts.bstd.as_deref(),
    };
    mk(settings, shell, &mkopts).map_err(creating(name, path))
}

/// `error` as what making the package `name` at `path` failed by.
fn creating<'a>(name: &'a str, path: &'a Path) -> impl FnOnce(NewError) -> NewError + 'a {
    move |error| NewError::Create { name: name.to_owned(), path: path.to_path_buf(), source: Box::new(error) }
}

pub fn init(
    opts: &NewOptions,
    settings: &Settings<'_>,
    shell: &mut impl Shell,
) -> Result<NewProjectKind, NewError> {
    let path = &opts.path;
    if settings.home.is_some_and(|home| path == home) {
        return Err(NewError::InHome);
    }
    let name = get_name(path, opts)?;
    let mut src_paths_types = vec![];
    detect_source_paths_and_types(path, name, &mut src_paths_types)?;
    let kind = calculate_new_project_kind(opts.kind, opts.auto_detect_kind, &src_paths_types);
    shell.status("Creating", &format!("{} package", opts.kind));

    if path.join("Cargo.toml").exists() {
        return Err(NewError::IsPackage);
    }
    check_path(path, shell);

    let has_bin = kind.is_bin();
    if src_paths_types.is_empty() {
        src_paths_types.push(plan_new_source_file(has_bin));
    } else if src_paths_types.len() == 1 && !src_paths_types.iter().any(|x| x.bin == has_bin) {
        // The only file found is not the type the user wants: change the type and warn.
        let file_type =
            if src_paths_types[0].bin { NewProjectKind::Bin } else { NewProjectKind::Lib };
        shell.warn(&format!(
            "file `{}` seems to be a {file_type} file",
            src_paths_types[0].relative_path
        ));
        src_paths_types[0].bin = has_bin
    } else if src_paths_types.len() > 1 && !has_bin {
        // Both lib and bin files were found and the user would like both treated as libs.
        return Err(NewError::TwoLibraries {
            first: src_paths_types[0].relative_path.clone(),
            second: src_paths_types[1].relative_path.clone(),
        });
    }

    check_name(name, opts.name.is_none(), has_bin, shell)?;

    let mut version_control = opts.version_control;
    if version_control.is_none() {
        let mut num_detected_vcses = 0;
        for (dir, vcs) in [
            (".git", VersionControl::Git),
            (".hg", VersionControl::Hg),
            (".pijul", VersionControl::Pijul),
            (".fossil", VersionControl::Fossil),
        ] {
            if path.join(dir).exists() {
                version_control = Some(vcs);
                num_detected_vcses += 1;
            }
        }
        if num_detected_vcses > 1 {
            return Err(NewError::ManyVcs);
        }
    }

    let mkopts = MkOptions {
        version_control,
        path,
        name,
        source_files: src_paths_types,
        edition: opts.edition.as_deref(),
        registry: opts.registry.as_deref(),
        bstd: opts.bstd.as_deref(),
    };
    mk(settings, shell, &mkopts).map_err(creating(name, path))?;
    Ok(kind)
}

/// The entries of an ignore file, in the forms of git, mercurial and fossil.
struct IgnoreList {
    ignore: Vec<String>,
    hg_ignore: Vec<String>,
    fossil_ignore: Vec<String>,
}

impl IgnoreList {
    fn new() -> IgnoreList {
        IgnoreList { ignore: Vec::new(), hg_ignore: Vec::new(), fossil_ignore: Vec::new() }
    }

    fn push(&mut self, ignore: &str, hg_ignore: &str, fossil_ignore: &str) {
        self.ignore.push(ignore.to_owned());
        self.hg_ignore.push(hg_ignore.to_owned());
        self.fossil_ignore.push(fossil_ignore.to_owned());
    }

    fn items(&self, vcs: VersionControl) -> &[String] {
        match vcs {
            VersionControl::Hg => &self.hg_ignore,
            VersionControl::Fossil => &self.fossil_ignore,
            _ => &self.ignore,
        }
    }

    /// The content of a new ignore file for `vcs`.
    fn format_new(&self, vcs: VersionControl) -> String {
        self.items(vcs).join("\n") + "\n"
    }

    /// What is appended to the ignore file `existing` for `vcs`: the entries it lacks, and
    /// those it has commented out.
    fn format_existing(&self, existing: &[u8], vcs: VersionControl) -> Result<String, NewError> {
        let mut existing_items = Vec::new();
        for (i, line) in existing.split(|it| *it == b'\n').enumerate() {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            match std::str::from_utf8(line) {
                Ok(s) => existing_items.push(s.to_owned()),
                Err(_) => return Err(NewError::IgnoreNotUtf8 { line: i }),
            }
        }
        let ignore_items = self.items(vcs);
        let mut out = String::new();
        // Fossil does not support `#` comments.
        if vcs != VersionControl::Fossil {
            out.push_str("\n\n# Added by cargo\n");
            if ignore_items.iter().any(|item| existing_items.contains(item)) {
                out.push_str("#\n# already existing elements were commented out\n");
            }
            out.push('\n');
        }
        for item in ignore_items {
            if existing_items.contains(item) {
                if vcs == VersionControl::Fossil {
                    // Just merge for Fossil.
                    continue;
                }
                out.push('#');
            }
            out.push_str(item);
            out.push('\n');
        }
        Ok(out)
    }
}

/// Writes the ignore file to `base_path`, or appends to the one there what it lacks.
fn write_ignore_file(
    base_path: &Path,
    list: &IgnoreList,
    vcs: VersionControl,
) -> Result<(), NewError> {
    // Fossil only supports project-level settings in a dedicated subdirectory.
    if vcs == VersionControl::Fossil {
        create_dir_all(&base_path.join(".fossil-settings"))?;
    }
    let files = match vcs {
        VersionControl::Git => vec![base_path.join(".gitignore")],
        VersionControl::Hg => vec![base_path.join(".hgignore")],
        VersionControl::Pijul => vec![base_path.join(".ignore")],
        // Fossil has a cleaning functionality configured in a separate file.
        VersionControl::Fossil => vec![
            base_path.join(".fossil-settings/ignore-glob"),
            base_path.join(".fossil-settings/clean-glob"),
        ],
        VersionControl::NoVcs => return Ok(()),
    };
    for fp_ignore in files {
        let ignore = match std::fs::read(&fp_ignore) {
            Err(err) if err.kind() == ErrorKind::NotFound => list.format_new(vcs),
            Err(err) => return Err(io_error(Doing::Open, &fp_ignore)(err)),
            Ok(existing) => list.format_existing(&existing, vcs)?,
        };
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&fp_ignore)
            .map_err(io_error(Doing::Open, &fp_ignore))?;
        file.write_all(ignore.as_bytes()).map_err(io_error(Doing::Append, &fp_ignore))?;
    }
    Ok(())
}

fn create_dir_all(path: &Path) -> Result<(), NewError> {
    std::fs::create_dir_all(path).map_err(io_error(Doing::CreateDir, path))
}

#[expect(clippy::disallowed_methods, reason = "each program is given the directory it runs in")]
fn command(program: &str, dir: &Path) -> Command {
    let mut command = Command::new(program);
    command.current_dir(dir);
    command
}

/// Runs a version control program as cargo runs it, its output shown, failing as cargo fails.
fn exec(program: &'static str, cwd: &Path, args: &[&std::ffi::OsStr]) -> Result<(), NewError> {
    let status = command(program, cwd).args(args).status().map_err(|error| NewError::Spawn { program, error })?;
    if !status.success() {
        return Err(NewError::Exited { program, status });
    }
    Ok(())
}

/// Whether `path` is in a git repository and not ignored there, or in a mercurial one: cargo's
/// `existing_vcs_repo`.
fn existing_vcs_repo(path: &Path, cwd: &Path) -> bool {
    let quiet = |program: &str, dir: &Path, args: &[&std::ffi::OsStr]| {
        command(program, dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|it| it.status.success())
    };
    let in_git_repo = || {
        if !path.is_dir() {
            return false;
        }
        let Some(found) = quiet("git", path, &["rev-parse".as_ref(), "--show-toplevel".as_ref()])
        else {
            return false;
        };
        let workdir = PathBuf::from(String::from_utf8_lossy(&found.stdout).trim_end_matches('\n'));
        // The working directory itself is not checked for being ignored.
        if workdir.canonicalize().ok() == path.canonicalize().ok() {
            return true;
        }
        let asked = ["check-ignore".as_ref(), "-q".as_ref(), "--".as_ref(), path.as_os_str()];
        quiet("git", &workdir, &asked).is_none()
    };
    let in_hg_repo = || {
        let asked = ["--cwd".as_ref(), path.as_os_str(), "root".as_ref()];
        quiet("hg", cwd, &asked).is_some()
    };
    in_git_repo() || in_hg_repo()
}

/// Makes the repository of `vcs` at `path`, unless there is one.
fn init_vcs(path: &Path, vcs: VersionControl, cwd: &Path) -> Result<(), NewError> {
    let os = |it: &Path| it.as_os_str().to_owned();
    match vcs {
        VersionControl::Git => {
            if !path.join(".git").exists() {
                create_dir_all(path)?;
                let done = command("git", path)
                    .args(["init", "--quiet"])
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|error| NewError::Spawn { program: "git", error })?;
                if !done.status.success() {
                    let stderr = String::from_utf8_lossy(&done.stderr).trim().to_owned();
                    return Err(NewError::GitInit { path: path.to_path_buf(), stderr });
                }
            }
        }
        VersionControl::Hg => {
            if !path.join(".hg").exists() {
                exec("hg", cwd, &["init".as_ref(), "--".as_ref(), &os(path)])?;
            }
        }
        VersionControl::Pijul => {
            if !path.join(".pijul").exists() {
                exec("pijul", cwd, &["init".as_ref(), "--".as_ref(), &os(path)])?;
            }
        }
        VersionControl::Fossil => {
            if !path.join(".fossil").exists() {
                // fossil doesn't create the directory.
                create_dir_all(path)?;
                let db_path = path.join(".fossil");
                exec("fossil", cwd, &["init".as_ref(), "--".as_ref(), &os(&db_path)])?;
                exec("fossil", path, &["open".as_ref(), "--".as_ref(), ".fossil".as_ref()])?;
            }
        }
        VersionControl::NoVcs => create_dir_all(path)?,
    }
    Ok(())
}

/// The manifest nearest `path` among its ancestors: cargo's `find_root_manifest_for_wd`.
fn find_root_manifest_for_wd(path: &Path) -> Option<PathBuf> {
    path.ancestors().map(|it| it.join("Cargo.toml")).find(|it| it.exists())
}

fn mk(settings: &Settings<'_>, shell: &mut impl Shell, opts: &MkOptions<'_>) -> Result<(), NewError> {
    let path = opts.path;
    let name = opts.name;

    // The entries of mutually-incompatible VCSs are kept in sync by pushing them together.
    let mut ignore = IgnoreList::new();
    ignore.push("/target", "^target$", "target");

    let vcs = opts.version_control.unwrap_or_else(|| {
        let in_existing_vcs = existing_vcs_repo(path.parent().unwrap_or(path), settings.cwd);
        match (settings.vcs, in_existing_vcs) {
            (None, false) => VersionControl::Git,
            (Some(opt), false) => opt,
            (_, true) => VersionControl::NoVcs,
        }
    });
    init_vcs(path, vcs, settings.cwd)?;
    write_ignore_file(path, &ignore, vcs)?;

    // `Cargo.toml`, with the `[lib]` and `[[bin]]` sections needed.
    let edition = opts.edition.unwrap_or(default_edition(opts.bstd.is_some()));
    // A blaze edition's, when the package is of one; a value that is no edition's is the
    // command line's to refuse, and is written as asked, as cargo writes it.
    let blaze = edition::BlazeEdition::from_manifest(edition).ok().flatten();
    if blaze.is_some() && !BSTD_SHIPS && opts.bstd.is_none() {
        return Err(NewError::NeedsBstd { edition: edition.to_owned() });
    }
    let mut manifest = NewManifest::new(name, edition, opts.registry);
    // The crate a blaze edition calls `std`, named until blaze ships it ([`BSTD_SHIPS`]).
    if let (Some(_), Some(bstd)) = (blaze, opts.bstd) {
        manifest.std = Some(bstd);
    }

    for i in &opts.source_files {
        if i.bin {
            if i.relative_path != "src/main.rs" {
                manifest.bins.push(i.relative_path.as_str());
            }
        } else if i.relative_path != "src/lib.rs" {
            manifest.lib = Some(i.relative_path.as_str());
        }
    }

    let manifest_path = blaze_manifest::paths::normalize_path(&path.join("Cargo.toml"));
    if let Some(root_manifest_path) = find_root_manifest_for_wd(&manifest_path) {
        let root_manifest =
            std::fs::read_to_string(&root_manifest_path).map_err(io_error(Doing::Read, &root_manifest_path))?;
        // The root manifest may not be valid; inheriting its workspace's keys is a best effort
        // that blocks nothing, and `check_workspace` below says why it does not read.
        if let Ok(workspace_document) = Document::parse_in(&root_manifest, Limits::CARGO, Global) {
            let display_path = get_display_path(&root_manifest_path, path)?;
            let workspace = workspace_document.root().get("workspace");
            let can_be_a_member = can_be_workspace_member(&display_path, &workspace_document)?;
            if can_be_a_member {
                // toml_edit's `as_table`: a `[workspace.package]` or dotted table, not an
                // inline one.
                if let Some(workspace_package_keys) = workspace
                    .and_then(|workspace| workspace.get("package"))
                    .filter(|package| package.is_table() && package.origin() != Origin::Inline)
                {
                    manifest.inherit(opts, workspace_package_keys);
                }
                if workspace.and_then(|workspace| workspace.get("lints")).is_some() {
                    manifest.lints = true;
                }
                if update_manifest_with_new_member(&root_manifest_path, &workspace_document, &display_path)? {
                    let member = Path::new(&display_path).file_name().and_then(|it| it.to_str());
                    let root = root_manifest_path.parent().unwrap_or(Path::new(""));
                    shell.status(
                        "Adding",
                        &format!(
                            "`{}` as member of workspace at `{}`",
                            member.unwrap_or_default(),
                            root.display()
                        ),
                    );
                }
            }
        }
    }

    std::fs::write(&manifest_path, manifest.text().as_str()).map_err(io_error(Doing::Write, &manifest_path))?;

    // The source files, with their directories, unless they are there.
    for i in &opts.source_files {
        let path_of_source_file = path.join(&i.relative_path);
        if let Some(src_dir) = path_of_source_file.parent() {
            create_dir_all(src_dir)?;
        }
        let default_file_content: &[u8] = if blaze.is_some() {
            match i.bin {
                true => BLAZE_MAIN.as_bytes(),
                false => BLAZE_LIB.as_bytes(),
            }
        } else if i.bin {
            b"\
fn main() {
    println!(\"Hello, world!\");
}
"
        } else {
            b"\
pub fn add(left: u64, right: u64) -> u64 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
"
        };
        if !path_of_source_file.is_file() {
            std::fs::write(&path_of_source_file, default_file_content)
                .map_err(io_error(Doing::Write, &path_of_source_file))?;
        }
    }

    if let Some(why) = (settings.check_workspace)(&manifest_path) {
        shell
            .warn("compiling this new package may not work due to invalid workspace configuration");
        shell.line("");
        shell.line(&why);
    }

    shell.note(
        "see more `Cargo.toml` keys and their definitions at https://doc.rust-lang.org/cargo/reference/manifest.html",
    );
    Ok(())
}

/// The new package's manifest, laid out as cargo's toml_edit lays out the new document it
/// builds: `[package]`'s keys in the order they are set (a key inherited from the workspace
/// taken out and put last, as `key.workspace = true`), then `[dependencies]`, each `[[bin]]`,
/// `[lib]` and `[lints]`, a blank line before every header after the first.
struct NewManifest<'a> {
    name: &'a str,
    /// Each key of `[package]`, with its value or, inherited, none.
    package: Vec<(String, Option<PackageValue<'a>>)>,
    /// Where bstd's package is, for the `std` dependency of a blaze edition.
    std: Option<&'a str>,
    /// The path of each `[[bin]]`, named as the package is.
    bins: Vec<&'a str>,
    lib: Option<&'a str>,
    /// `[lints] workspace = true`.
    lints: bool,
}

/// A value of the new `[package]`.
enum PackageValue<'a> {
    Text(&'a str),
    /// `publish = ["<registry>"]`.
    Registry(&'a str),
}

impl<'a> NewManifest<'a> {
    fn new(name: &'a str, edition: &'a str, registry: Option<&'a str>) -> NewManifest<'a> {
        let mut package = vec![
            ("name".to_owned(), Some(PackageValue::Text(name))),
            ("version".to_owned(), Some(PackageValue::Text("0.1.0"))),
            ("edition".to_owned(), Some(PackageValue::Text(edition))),
        ];
        if let Some(registry) = registry {
            package.push(("publish".to_owned(), Some(PackageValue::Registry(registry))));
        }
        NewManifest { name, package, std: None, bins: Vec::new(), lib: None, lints: false }
    }

    /// Each key of the workspace's `[workspace.package]` inherited, but an edition or registry
    /// the command line names.
    fn inherit(&mut self, opts: &MkOptions<'_>, workspace_package_keys: Item<'_, '_, Global>) {
        for key in workspace_package_keys.children().map(|it| it.key()) {
            if key == "edition" && opts.edition.is_some() {
                continue;
            }
            if key == "publish" && opts.registry.is_some() {
                continue;
            }
            self.package.retain(|(it, _)| it != key);
            self.package.push((key.to_owned(), None));
        }
    }

    fn text(&self) -> blaze_own::String<Global> {
        let mut out = blaze_own::String::new_in(Global);
        out.push_str("[package]\n");
        for (key, value) in &self.package {
            blaze_toml::write_key(key, &mut out);
            match value {
                Some(PackageValue::Text(text)) => {
                    out.push_str(" = ");
                    write_string(text, &mut out);
                }
                Some(PackageValue::Registry(registry)) => {
                    out.push_str(" = [");
                    write_string(registry, &mut out);
                    out.push(']');
                }
                None => out.push_str(".workspace = true"),
            }
            out.push('\n');
        }
        out.push_str("\n[dependencies]\n");
        if let Some(bstd) = self.std {
            blaze_toml::write_key(edition::BlazeEdition::STD, &mut out);
            out.push_str(" = { package = ");
            write_string("blaze-std", &mut out);
            out.push_str(", path = ");
            write_string(bstd, &mut out);
            out.push_str(" }\n");
        }
        for path in &self.bins {
            out.push_str("\n[[bin]]\nname = ");
            write_string(self.name, &mut out);
            out.push_str("\npath = ");
            write_string(path, &mut out);
            out.push('\n');
        }
        if let Some(path) = self.lib {
            out.push_str("\n[lib]\npath = ");
            write_string(path, &mut out);
            out.push('\n');
        }
        if self.lints {
            out.push_str("\n[lints]\nworkspace = true\n");
        }
        out
    }
}

fn text_value(text: &str) -> Value<Global> {
    Value::Text(blaze_own::String::from_str_in(text, Global))
}

/// The text of `item` as `doc` writes it, for a message.
fn written(doc: &Document<'_, Global>, item: Item<'_, '_, Global>) -> String {
    doc.text()[item.span()].to_owned()
}

/// Adds the new package to `[workspace.members]`, unless a pattern there matches it already;
/// with no `members`, makes them. Whether the root manifest was written.
fn update_manifest_with_new_member(
    root_manifest_path: &Path,
    workspace_document: &Document<'_, Global>,
    display_path: &str,
) -> Result<bool, NewError> {
    let Some(workspace) = workspace_document.root().get("workspace") else {
        return Ok(false);
    };
    let edit_error = |error| NewError::Edit { path: root_manifest_path.to_path_buf(), error };
    let mut edit = blaze_toml::Edit::new_in(workspace_document, Global);
    // toml_edit's `as_array_mut`: an array written `[ ]`, not one of tables.
    match workspace.get("members").filter(|members| members.is_array() && members.origin() == Origin::Value) {
        Some(members) => {
            let mut pats = Vec::with_capacity(members.len());
            for member in members.children() {
                let Some(pat) = member.as_str() else {
                    return Err(NewError::NotAString { exclude: false, written: written(workspace_document, member) });
                };
                let pattern = blaze_glob::Pattern::new(pat)
                    .map_err(|error| NewError::Pattern { pattern: pat.to_owned(), error })?;
                if pattern.matches(display_path) {
                    return Ok(false);
                }
                pats.push(pat);
            }
            // cargo pushes and then sorts a sorted list, stably: the new member goes after every
            // member not above it.
            let index = match pats.is_sorted() {
                true => pats.iter().position(|it| *it > display_path).unwrap_or(pats.len()),
                false => pats.len(),
            };
            edit.insert(members, index, &text_value(display_path)).map_err(edit_error)?;
        }
        None => {
            let mut list = blaze_own::Vec::with_capacity_in(1, Global);
            list.push(text_value(display_path));
            edit.set(workspace, "members", &Value::List(list)).map_err(edit_error)?;
        }
    }
    let text = edit.finish().map_err(edit_error)?;
    write_atomic(root_manifest_path, text.as_str().as_bytes())?;
    Ok(true)
}

/// Writes `contents` to `path` by a file beside it renamed over it.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), NewError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let temp = dir.join(format!(".{}.blaze-new", std::process::id()));
    std::fs::write(&temp, contents).map_err(io_error(Doing::Write, &temp))?;
    let meta = std::fs::metadata(path).map_err(io_error(Doing::Read, path))?;
    std::fs::set_permissions(&temp, meta.permissions()).map_err(io_error(Doing::Write, &temp))?;
    std::fs::rename(&temp, path).map_err(io_error(Doing::Write, path))
}

/// The package's path relative to the workspace root, by `/`.
fn get_display_path(root_manifest_path: &Path, package_path: &Path) -> Result<String, NewError> {
    let workspace_root =
        root_manifest_path.parent().ok_or_else(|| NewError::NoParent { path: root_manifest_path.to_path_buf() })?;
    let relpath = pathdiff::diff_paths(package_path, workspace_root).ok_or_else(|| NewError::NotRelative {
        package: package_path.to_path_buf(),
        workspace: workspace_root.to_path_buf(),
    })?;
    let mut components = Vec::new();
    for comp in relpath.iter() {
        let comp = comp.to_str().ok_or_else(|| NewError::NonUnicodePath { path: relpath.clone() })?;
        components.push(comp);
    }
    Ok(components.join("/"))
}

/// Whether `[workspace.exclude]` leaves the package out.
fn can_be_workspace_member(display_path: &str, workspace_document: &Document<'_, Global>) -> Result<bool, NewError> {
    // toml_edit's `as_array`: an array written `[ ]`, not one of tables.
    if let Some(exclude) = workspace_document
        .root()
        .get("workspace")
        .and_then(|workspace| workspace.get("exclude"))
        .filter(|exclude| exclude.is_array() && exclude.origin() == Origin::Value)
    {
        for member in exclude.children() {
            let Some(pat) = member.as_str() else {
                return Err(NewError::NotAString { exclude: true, written: written(workspace_document, member) });
            };
            if pat == display_path {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
