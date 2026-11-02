//! `cargo new` and `cargo init`, made by blaze with no cargo: `src/ops/cargo_new.rs` and
//! `src/util/vcs.rs` of cargo 1.101.0 (cad07cf8e5). It writes what cargo writes, byte for byte:
//! the manifest, the source file, the ignore file, the repository, and the workspace's
//! `members`. cargo runs rustfmt on the source file it writes, which is formatted already;
//! blaze runs no rustfmt. A git repository is made by `git init`, where cargo has libgit2.

use std::collections::BTreeMap;
use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, anyhow, bail};
use cargo_util_schemas::manifest::PackageName;
use toml_edit::{Array, Value};

/// The edition `cargo new` writes when none is asked.
const LATEST_STABLE: &str = "2024";

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
    ) -> anyhow::Result<NewOptions> {
        let auto_detect_kind = !bin && !lib;
        let kind = match (bin, lib) {
            (true, true) => bail!("can't specify both lib and binary outputs"),
            (false, true) => NewProjectKind::Lib,
            (_, false) => NewProjectKind::Bin,
        };
        Ok(NewOptions { version_control, kind, auto_detect_kind, path, name, edition, registry })
    }
}

fn get_name<'a>(path: &'a Path, opts: &'a NewOptions) -> anyhow::Result<&'a str> {
    if let Some(ref name) = opts.name {
        return Ok(name);
    }
    let file_name = path.file_name().ok_or_else(|| {
        anyhow!(
            "cannot auto-detect package name from path {:?} ; use --name to override",
            path.as_os_str()
        )
    })?;
    file_name
        .to_str()
        .ok_or_else(|| anyhow!("cannot create package with a non-unicode name: {file_name:?}"))
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
) -> anyhow::Result<()> {
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
    PackageName::new(name).map_err(|err| anyhow!("{err}{}", bin_help()))?;

    if is_keyword(name) {
        bail!("invalid package name `{name}`: it is a Rust keyword{}", bin_help());
    }
    if is_conflicting_artifact_name(name) {
        if has_bin {
            bail!(
                "invalid package name `{name}`: it conflicts with cargo's build directory names{name_help}"
            );
        }
        shell.warn(&format!(
            "package `{name}` will not support binary executables with that name, it conflicts with cargo's build directory names"
        ));
    }
    if name == "test" {
        bail!(
            "invalid package name `test`: it conflicts with Rust's built-in test library{}",
            bin_help()
        );
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
            bail!("invalid package name `{name}`: it is a reserved Windows filename{name_help}");
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
) -> anyhow::Result<()> {
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
            H::Detect => std::fs::read_to_string(path.join(&pp))
                .with_context(|| format!("failed to read `{}`", path.join(&pp).display()))?
                .contains("fn main"),
        };
        detected_files.push(SourceFileInformation { relative_path: pp, bin });
    }

    // Check for duplicate lib attempt.
    let mut previous_lib_relpath: Option<&str> = None;
    let mut duplicates_checker: BTreeMap<&str, &SourceFileInformation> = BTreeMap::new();
    for i in detected_files.iter() {
        if i.bin {
            if let Some(x) = duplicates_checker.get(name) {
                bail!(
                    "\
multiple possible binary sources found:
  {}
  {}
cannot automatically generate Cargo.toml as the main target would be ambiguous",
                    x.relative_path,
                    i.relative_path
                );
            }
            duplicates_checker.insert(name, i);
        } else {
            if let Some(plp) = previous_lib_relpath {
                bail!(
                    "cannot have a package with multiple libraries, found both `{plp}` and `{}`",
                    i.relative_path
                )
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
) -> anyhow::Result<()> {
    let path = &opts.path;
    let name = get_name(path, opts)?;
    shell.status("Creating", &format!("{} `{name}` package", opts.kind));

    if path.exists() {
        bail!(
            "destination `{}` already exists\n\n\
             Use `cargo init` to initialize the directory",
            path.display()
        )
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
    };
    mk(settings, shell, &mkopts)
        .with_context(|| format!("failed to create package `{name}` at `{}`", path.display()))
}

pub fn init(
    opts: &NewOptions,
    settings: &Settings<'_>,
    shell: &mut impl Shell,
) -> anyhow::Result<NewProjectKind> {
    let path = &opts.path;
    if settings.home.is_some_and(|home| path == home) {
        bail!(
            "cannot create package in the home directory\n\n\
             help: use `cargo init <path>` to create a package in a different directory"
        )
    }
    let name = get_name(path, opts)?;
    let mut src_paths_types = vec![];
    detect_source_paths_and_types(path, name, &mut src_paths_types)?;
    let kind = calculate_new_project_kind(opts.kind, opts.auto_detect_kind, &src_paths_types);
    shell.status("Creating", &format!("{} package", opts.kind));

    if path.join("Cargo.toml").exists() {
        bail!(
            "`cargo init` cannot be run on existing Cargo packages\n\
             help: use `cargo new` to create a package in a new subdirectory"
        )
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
        bail!(
            "cannot have a package with multiple libraries, found both `{}` and `{}`",
            src_paths_types[0].relative_path,
            src_paths_types[1].relative_path
        )
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
            bail!(
                "more than one of .hg, .git, .pijul, .fossil configurations found and the ignore file can't be filled in as a result. specify --vcs to override detection"
            );
        }
    }

    let mkopts = MkOptions {
        version_control,
        path,
        name,
        source_files: src_paths_types,
        edition: opts.edition.as_deref(),
        registry: opts.registry.as_deref(),
    };
    mk(settings, shell, &mkopts)
        .with_context(|| format!("failed to create package `{name}` at `{}`", path.display()))?;
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
    fn format_existing(&self, existing: &[u8], vcs: VersionControl) -> anyhow::Result<String> {
        let mut existing_items = Vec::new();
        for (i, line) in existing.split(|it| *it == b'\n').enumerate() {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            match std::str::from_utf8(line) {
                Ok(s) => existing_items.push(s.to_owned()),
                Err(_) => bail!("Character at line {i} is invalid. Cargo only supports UTF-8."),
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
) -> anyhow::Result<()> {
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
            Err(err) => {
                return Err(anyhow::Error::new(err)
                    .context(format!("failed to open: {}", fp_ignore.display())));
            }
            Ok(existing) => list.format_existing(&existing, vcs)?,
        };
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&fp_ignore)
            .with_context(|| format!("failed to open: {}", fp_ignore.display()))?;
        file.write_all(ignore.as_bytes())
            .with_context(|| format!("failed to write: {}", fp_ignore.display()))?;
    }
    Ok(())
}

fn create_dir_all(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create directory `{}`", path.display()))
}

#[expect(clippy::disallowed_methods, reason = "each program is given the directory it runs in")]
fn command(program: &str, dir: &Path) -> Command {
    let mut command = Command::new(program);
    command.current_dir(dir);
    command
}

/// Runs a version control program as cargo runs it, its output shown, failing as cargo fails.
fn exec(program: &str, cwd: &Path, args: &[&std::ffi::OsStr]) -> anyhow::Result<()> {
    let status = command(program, cwd)
        .args(args)
        .status()
        .with_context(|| format!("could not execute process `{program}` (never executed)"))?;
    if !status.success() {
        bail!("process didn't exit successfully: `{program}` ({status})");
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
fn init_vcs(path: &Path, vcs: VersionControl, cwd: &Path) -> anyhow::Result<()> {
    let os = |it: &Path| it.as_os_str().to_owned();
    match vcs {
        VersionControl::Git => {
            if !path.join(".git").exists() {
                create_dir_all(path)?;
                let done = command("git", path)
                    .args(["init", "--quiet"])
                    .stdin(Stdio::null())
                    .output()
                    .context("could not execute process `git` (never executed)")?;
                if !done.status.success() {
                    bail!(
                        "failed to make a git repository at `{}`: {}",
                        path.display(),
                        String::from_utf8_lossy(&done.stderr).trim()
                    );
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

fn mk(settings: &Settings<'_>, shell: &mut impl Shell, opts: &MkOptions<'_>) -> anyhow::Result<()> {
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
    let mut manifest = toml_edit::DocumentMut::new();
    manifest["package"] = toml_edit::Item::Table(toml_edit::Table::new());
    manifest["package"]["name"] = toml_edit::value(name);
    manifest["package"]["version"] = toml_edit::value("0.1.0");
    manifest["package"]["edition"] = toml_edit::value(opts.edition.unwrap_or(LATEST_STABLE));
    if let Some(registry) = opts.registry {
        let mut array = toml_edit::Array::default();
        array.push(registry);
        manifest["package"]["publish"] = toml_edit::value(array);
    }
    manifest["dependencies"] = toml_edit::Item::Table(toml_edit::Table::default());

    for i in &opts.source_files {
        if i.bin {
            if i.relative_path != "src/main.rs" {
                let mut bin = toml_edit::Table::new();
                bin["name"] = toml_edit::value(name);
                bin["path"] = toml_edit::value(i.relative_path.clone());
                manifest["bin"]
                    .or_insert(toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()))
                    .as_array_of_tables_mut()
                    .expect("bin is an array of tables")
                    .push(bin);
            }
        } else if i.relative_path != "src/lib.rs" {
            let mut lib = toml_edit::Table::new();
            lib["path"] = toml_edit::value(i.relative_path.clone());
            manifest["lib"] = toml_edit::Item::Table(lib);
        }
    }

    let manifest_path = blaze_manifest::paths::normalize_path(&path.join("Cargo.toml"));
    if let Some(root_manifest_path) = find_root_manifest_for_wd(&manifest_path) {
        let root_manifest = std::fs::read_to_string(&root_manifest_path)
            .with_context(|| format!("failed to read `{}`", root_manifest_path.display()))?;
        // The root manifest may not be valid; inheriting its workspace's keys is a best effort
        // that blocks nothing.
        if let Ok(mut workspace_document) = root_manifest.parse::<toml_edit::DocumentMut>() {
            let display_path = get_display_path(&root_manifest_path, path)?;
            let can_be_a_member = can_be_workspace_member(&display_path, &workspace_document)?;
            if can_be_a_member {
                if let Some(workspace_package_keys) = workspace_document
                    .get("workspace")
                    .and_then(|workspace| workspace.get("package"))
                    .and_then(|package| package.as_table())
                {
                    inherit_workspace_package_keys(opts, &mut manifest, workspace_package_keys)
                }
                if workspace_document
                    .get("workspace")
                    .and_then(|workspace| workspace.get("lints"))
                    .is_some()
                {
                    let mut table = toml_edit::Table::new();
                    table["workspace"] = toml_edit::value(true);
                    manifest["lints"] = toml_edit::Item::Table(table);
                }
                if update_manifest_with_new_member(
                    &root_manifest_path,
                    &mut workspace_document,
                    &display_path,
                )? {
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

    std::fs::write(&manifest_path, manifest.to_string())
        .with_context(|| format!("failed to write `{}`", manifest_path.display()))?;

    // The source files, with their directories, unless they are there.
    for i in &opts.source_files {
        let path_of_source_file = path.join(&i.relative_path);
        if let Some(src_dir) = path_of_source_file.parent() {
            create_dir_all(src_dir)?;
        }
        let default_file_content: &[u8] = if i.bin {
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
                .with_context(|| format!("failed to write `{}`", path_of_source_file.display()))?;
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

/// Has the new manifest inherit each key of the workspace's `[workspace.package]`, but an
/// edition or registry the command line names.
fn inherit_workspace_package_keys(
    opts: &MkOptions<'_>,
    manifest: &mut toml_edit::DocumentMut,
    workspace_package_keys: &toml_edit::Table,
) {
    for (key, _) in workspace_package_keys {
        if key == "edition" && opts.edition.is_some() {
            continue;
        }
        if key == "publish" && opts.registry.is_some() {
            continue;
        }
        let package = manifest["package"].as_table_mut().expect("package is a table");
        package.remove(key);
        let mut table = toml_edit::Table::new();
        table.set_dotted(true);
        table["workspace"] = toml_edit::value(true);
        package.insert(key, toml_edit::Item::Table(table));
    }
}

/// Adds the new package to `[workspace.members]`, unless a pattern there matches it already;
/// with no `members`, makes them. Whether the root manifest was written.
fn update_manifest_with_new_member(
    root_manifest_path: &Path,
    workspace_document: &mut toml_edit::DocumentMut,
    display_path: &str,
) -> anyhow::Result<bool> {
    let Some(workspace) = workspace_document.get_mut("workspace") else {
        return Ok(false);
    };
    if let Some(members) = workspace.get_mut("members").and_then(|members| members.as_array_mut()) {
        for member in members.iter() {
            let pat =
                member.as_str().with_context(|| format!("invalid non-string member `{member}`"))?;
            let pattern = glob::Pattern::new(pat)
                .with_context(|| format!("cannot build glob pattern from `{pat}`"))?;
            if pattern.matches(display_path) {
                return Ok(false);
            }
        }
        let was_sorted = members.iter().map(Value::as_str).is_sorted();
        members.push(display_path);
        if was_sorted {
            members.sort_by(|lhs, rhs| lhs.as_str().cmp(&rhs.as_str()));
        }
    } else {
        let mut array = Array::new();
        array.push(display_path);
        workspace["members"] = toml_edit::value(array);
    }
    write_atomic(root_manifest_path, workspace_document.to_string().as_bytes())?;
    Ok(true)
}

/// Writes `contents` to `path` by a file beside it renamed over it.
fn write_atomic(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let temp = dir.join(format!(".{}.blaze-new", std::process::id()));
    std::fs::write(&temp, contents)
        .with_context(|| format!("failed to write `{}`", temp.display()))?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&temp, meta.permissions());
    }
    std::fs::rename(&temp, path).with_context(|| format!("failed to write `{}`", path.display()))
}

/// The package's path relative to the workspace root, by `/`.
fn get_display_path(root_manifest_path: &Path, package_path: &Path) -> anyhow::Result<String> {
    let workspace_root = root_manifest_path.parent().with_context(|| {
        format!(
            "workspace root manifest doesn't have a parent directory `{}`",
            root_manifest_path.display()
        )
    })?;
    let relpath = pathdiff::diff_paths(package_path, workspace_root).with_context(|| {
        format!(
            "path comparison requires two absolute paths; package_path: `{}`, workspace_path: `{}`",
            package_path.display(),
            workspace_root.display()
        )
    })?;
    let mut components = Vec::new();
    for comp in relpath.iter() {
        let comp = comp.to_str().with_context(|| {
            format!("invalid unicode component in path `{}`", relpath.display())
        })?;
        components.push(comp);
    }
    Ok(components.join("/"))
}

/// Whether `[workspace.exclude]` leaves the package out.
fn can_be_workspace_member(
    display_path: &str,
    workspace_document: &toml_edit::DocumentMut,
) -> anyhow::Result<bool> {
    if let Some(exclude) = workspace_document
        .get("workspace")
        .and_then(|workspace| workspace.get("exclude"))
        .and_then(|exclude| exclude.as_array())
    {
        for member in exclude {
            let pat = member
                .as_str()
                .with_context(|| format!("invalid non-string exclude path `{member}`"))?;
            if pat == display_path {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
