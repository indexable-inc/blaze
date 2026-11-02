//! The environment cargo 1.99 runs a build script with: `Compilation::host_process` and
//! `fill_env` (`src/compiler/compilation.rs`), then `build_work`
//! (`src/compiler/custom_build.rs`) and the jobserver, in rust-lang/cargo at 5f94df478, the
//! commit `cargo 1.99.0 -vV` names. The script inherits cargo's own environment with what
//! these set and take away.
//!
//! Left out, as unstable in 1.99 and so never set by a stable cargo: `CARGO_DEP_<name>_<key>`
//! (`-Z any-build-script-metadata`), `CARGO_TRIM_PATHS_*` (`-Z trim-paths`), the artifact
//! dependencies' variables (`-Z bindeps`) and the host runner (`-Z host-config`).
//!
//! Built once per unit: what cargo sets is a list in the order set, names of the fixed set
//! `&'static str` and every other name and value made at its size in one allocation; it is
//! sorted once, by a precomputed `u128` of a name's first bytes, the last word on each name
//! kept, and merged with what cargo inherits, which is sorted only when the caller did not
//! pass it sorted. What is inherited is copied only where it is kept.

use std::{
    borrow::Cow,
    cmp::Ordering,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use crate::{
    Error,
    unit::{Package, Unfilled, Unit},
};

/// A script's whole environment, and which of it cargo sets from the unit.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Env {
    /// Sorted by name, each name once: read in this order into the run's key and the
    /// child's environment.
    pub vars: Vec<(OsString, OsString)>,
    /// Whether each of `vars`, by index, is one cargo sets from the unit, its configuration
    /// and the build (#805): `CARGO_*`, `OUT_DIR`, `TARGET`, `HOST`, the profile's, the
    /// tools', the `DEP_*` of its dependencies, `[env]`'s, the dynamic library path (with
    /// what cargo's own environment had of it after the unit's directories), the
    /// jobserver's `CARGO_MAKEFLAGS`. Every other variable is passed through from the
    /// environment cargo was started with.
    from_unit: Vec<bool>,
}

/// Where a variable of a script's environment comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Set by cargo from the unit: what a run is given.
    Unit,
    /// Passed through from the environment cargo was started with: what a run reads of it
    /// is traced.
    PassedThrough,
}

impl Env {
    fn find(&self, name: &OsStr) -> Option<usize> {
        self.vars.binary_search_by(|(it, _)| it.as_os_str().cmp(name)).ok()
    }

    pub fn get(&self, name: &str) -> Option<&OsStr> {
        self.find(OsStr::new(name)).map(|at| self.vars[at].1.as_os_str())
    }

    /// Where `name` comes from, if the script has it.
    pub fn origin(&self, name: &OsStr) -> Option<Origin> {
        self.find(name).map(|at| match self.from_unit[at] {
            true => Origin::Unit,
            false => Origin::PassedThrough,
        })
    }

    /// The variables set from the unit, sorted by name.
    pub fn set_from_unit(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.with_origin(true)
    }

    /// The variables passed through from cargo's environment, sorted by name.
    pub fn passed_through(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.with_origin(false)
    }

    fn with_origin(&self, from_unit: bool) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.vars
            .iter()
            .zip(&self.from_unit)
            .filter(move |(_, it)| **it == from_unit)
            .map(|((name, value), _)| (name.as_os_str(), value.as_os_str()))
    }

}

/// What cargo sets on the command, in order, over what it inherits: a variable set, or taken
/// away (`None`). The last word on a name is the one that counts.
struct Explicit {
    vars: Vec<(Cow<'static, str>, Option<OsString>)>,
}

impl Explicit {
    fn set(&mut self, name: impl Into<Cow<'static, str>>, value: impl Into<OsString>) {
        self.vars.push((name.into(), Some(value.into())));
    }

    fn remove(&mut self, name: &'static str) {
        self.vars.push((Cow::Borrowed(name), None));
    }

    fn contains(&self, name: &str) -> bool {
        self.vars.iter().any(|(it, _)| it == name)
    }

    fn get(&self, name: &str) -> Option<&OsStr> {
        self.vars.iter().rev().find(|(it, _)| it == name).and_then(|(_, it)| it.as_deref())
    }
}

/// What the machine that runs the script is, as far as its environment goes: cargo decides
/// these by what it was built for, which is the host.
struct HostOs {
    windows: bool,
    macos: bool,
    /// The variable programs find dynamic libraries by.
    dylib_var: &'static str,
}

impl HostOs {
    fn of(host: &str) -> HostOs {
        let windows = host.contains("windows");
        let macos = host.contains("apple");
        let dylib_var = if windows {
            "PATH"
        } else if macos {
            "DYLD_FALLBACK_LIBRARY_PATH"
        } else if host.contains("aix") {
            "LIBPATH"
        } else if host.contains("haiku") {
            "LIBRARY_PATH"
        } else {
            "LD_LIBRARY_PATH"
        };
        HostOs { windows, macos, dylib_var }
    }

    /// `std::env::split_paths` on the host: on Unix the bytes split at `:`, as std splits
    /// them, so a path that is not UTF-8 is kept as it is.
    fn split_paths(&self, value: &OsStr) -> Vec<PathBuf> {
        if !self.windows {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                return value
                    .as_bytes()
                    .split(|it| *it == b':')
                    .map(|it| PathBuf::from(OsStr::from_bytes(it)))
                    // alloc: setup, once per unit: the library path, split to be joined again.
                    .collect();
            }
            #[cfg(not(unix))]
            // alloc: setup, once per unit: the library path, split to be joined again.
            return value.to_string_lossy().split(':').map(PathBuf::from).collect();
        }
        let value = value.to_string_lossy();
        // alloc: setup, once per unit: the library path, split to be joined again.
        let mut paths = Vec::new();
        // alloc: setup, once per unit: one buffer for each path's text.
        let mut path = String::new();
        let mut quoted = false;
        let mut after_separator = false;
        for c in value.chars() {
            after_separator = false;
            match c {
                '"' => quoted = !quoted,
                ';' if !quoted => {
                    paths.push(PathBuf::from(std::mem::take(&mut path)));
                    after_separator = true;
                }
                c => path.push(c),
            }
        }
        if after_separator || !path.is_empty() {
            paths.push(PathBuf::from(path));
        }
        paths
    }

    /// `std::env::join_paths` on the host, which cargo fails on as here. The separators and
    /// quotes looked for are ASCII, which a path's encoded bytes hold only as themselves.
    fn join_paths(&self, paths: &[PathBuf], var: &str) -> Result<OsString, Error> {
        let len = paths.iter().map(|it| it.as_os_str().len() + 3).sum();
        let mut joined = OsString::with_capacity(len);
        for (i, path) in paths.iter().enumerate() {
            let text = path.as_os_str();
            let bytes = text.as_encoded_bytes();
            if i > 0 {
                joined.push(if self.windows { ";" } else { ":" });
            }
            if self.windows && bytes.contains(&b'"') || !self.windows && bytes.contains(&b':') {
                // alloc: error path.
                let mut message = format!(
                    "failed to join paths from `${var}` together\n\n\
                     Check if any of path segments listed below contain an \
                     unterminated quote character or path separator:"
                );
                for path in paths {
                    // alloc: error path.
                    message.push_str(&format!("\n    {path:?}"));
                }
                return Err(Error(message));
            }
            if self.windows && bytes.contains(&b';') {
                joined.push("\"");
                joined.push(text);
                joined.push("\"");
            } else {
                joined.push(text);
            }
        }
        Ok(joined)
    }
}

/// `name` as cargo makes it part of a variable's, added to `out`: upper case, `-` as `_`.
fn envify_into(out: &mut String, name: &str) {
    if name.is_ascii() {
        out.extend(name.bytes().map(|it| match it {
            b'-' => '_',
            it => char::from(it.to_ascii_uppercase()),
        }));
    } else {
        out.extend(
            name.chars().flat_map(char::to_uppercase).map(|c| if c == '-' { '_' } else { c }),
        );
    }
}

/// A name as cargo makes it part of a variable's: upper case, `-` as `_`.
pub fn envify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    envify_into(&mut out, name);
    out
}

/// `prefix` and `name` [`envify`]d, in one allocation of their size.
fn prefixed(prefix: &str, name: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + name.len());
    out.push_str(prefix);
    envify_into(&mut out, name);
    out
}

/// `n` in decimal, without the formatting machinery.
fn decimal(mut n: u32) -> String {
    let mut digits = [0u8; 10];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    // alloc: setup, once per unit: a number made at its length.
    digits[at..].iter().map(|it| char::from(*it)).collect()
}

/// A cfg as `rustc --print cfg` prints it: a name, or a name and a quoted value.
fn cfg_of<'l>(package: &Package, line: &'l str) -> Result<(&'l str, Option<&'l str>), Unfilled> {
    match line.split_once('=') {
        Some((name, value)) => {
            let value = value.trim();
            match value.strip_prefix('"').and_then(|it| it.strip_suffix('"')) {
                Some(value) => Ok((name.trim(), Some(value))),
                // alloc: error path.
                None => Err(Unfilled::Cfg { package: package.descr(), line: line.to_owned() }),
            }
        }
        None => Ok((line.trim(), None)),
    }
}

/// The cfgs as cargo's map has them, each name where it was first named: a name's values
/// are those given since it was last given a whole list. Three vectors, not one per name.
#[derive(Default)]
struct Cfgs<'a> {
    names: Vec<&'a str>,
    /// Per name, the first of `values` that can still be its own.
    since: Vec<u32>,
    /// Each value, with the index of its name.
    values: Vec<(u32, &'a str)>,
}

impl<'a> Cfgs<'a> {
    fn slot(&mut self, name: &'a str) -> u32 {
        match self.names.iter().position(|it| *it == name) {
            Some(at) => at as u32,
            None => {
                self.names.push(name);
                self.since.push(self.values.len() as u32);
                (self.names.len() - 1) as u32
            }
        }
    }

    /// `name` with exactly `values`.
    fn insert(&mut self, name: &'a str, values: impl IntoIterator<Item = &'a str>) {
        let slot = self.slot(name);
        self.since[slot as usize] = self.values.len() as u32;
        self.values.extend(values.into_iter().map(|it| (slot, it)));
    }

    fn add(&mut self, name: &'a str, value: &'a str) {
        let slot = self.slot(name);
        self.values.push((slot, value));
    }

    /// Each name, in order, with its values joined by `,`.
    fn joined(&self) -> impl Iterator<Item = (&'a str, String)> + '_ {
        self.names.iter().enumerate().map(|(slot, name)| {
            let own = || {
                self.values[self.since[slot] as usize..]
                    .iter()
                    .filter(move |(it, _)| *it as usize == slot)
                    .map(|(_, value)| *value)
            };
            let len = own().map(|it| it.len() + 1).sum::<usize>().saturating_sub(1);
            let mut joined = String::with_capacity(len);
            for (i, value) in own().enumerate() {
                if i > 0 {
                    joined.push(',');
                }
                joined.push_str(value);
            }
            (*name, joined)
        })
    }
}

/// The search path of `paths` for dynamic libraries: of those with a kind (`native=`,
/// `crate=`, `dependency=`, `framework=`, `all=`), the directory.
fn dynamic_search_path(path: &Path) -> &Path {
    match path.to_str().and_then(|it| it.split_once('=')) {
        Some(("native" | "crate" | "dependency" | "framework" | "all", path)) => Path::new(path),
        _ => path,
    }
}

/// Of `paths`, the directories below `root`: one outside is likely the system's, whose
/// libraries would be found in place of the system's own (cargo's issue 3366).
fn below(paths: &[PathBuf], root: &Path) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|it| dynamic_search_path(it))
        .filter(|it| it.starts_with(root))
        .map(Path::to_owned)
        // alloc: setup, once per unit: the plugin directories it keeps.
        .collect()
}

/// Where openssl-probe 0.1.6 looks for certificates, in order, on `host`.
fn certificate_dirs(host: &str) -> Vec<&'static Path> {
    // alloc: setup: once per cargo started, the directories openssl-probe looks in.
    let mut dirs = vec![
        "/var/ssl",
        "/usr/share/ssl",
        "/usr/local/ssl",
        "/usr/local/openssl",
        "/usr/local/etc/openssl",
        "/usr/local/share",
        "/usr/lib/ssl",
        "/usr/ssl",
        "/etc/openssl",
        "/etc/pki/ca-trust/extracted/pem",
        "/etc/pki/tls",
        "/etc/ssl",
        "/etc/certs",
        "/opt/etc/ssl",
    ];
    if host.contains("android") {
        dirs.push("/data/data/com.termux/files/usr/etc/tls");
    }
    if host.contains("haiku") {
        dirs.push("/boot/system/data/ssl");
    }
    // alloc: setup: once per cargo started.
    dirs.into_iter().map(Path::new).collect()
}

const CERTIFICATE_FILES: [&str; 10] = [
    "cert.pem",
    "certs.pem",
    "ca-bundle.pem",
    "cacert.pem",
    "ca-certificates.crt",
    "certs/ca-certificates.crt",
    "certs/ca-root-nss.crt",
    "certs/ca-bundle.crt",
    "CARootCertificates.pem",
    "tls-ca-bundle.pem",
];

/// cargo's environment once it has started, from the one it was started with: what its
/// scripts inherit. Before any command, cargo starts libgit2, whose `openssl_env_init` (git2
/// 0.21.0, on hosts other than Windows, macOS and iOS) sets `SSL_CERT_FILE` and `SSL_CERT_DIR`
/// to the certificates openssl-probe 0.1.6 finds on this machine, unless they name a file and
/// a directory that exist.
pub fn cargo_started(
    started_with: &[(OsString, OsString)],
    host: &str,
) -> Vec<(OsString, OsString)> {
    cargo_started_in(started_with, host, &certificate_dirs(host))
}

fn cargo_started_in(
    started_with: &[(OsString, OsString)],
    host: &str,
    certificate_dirs: &[&Path],
) -> Vec<(OsString, OsString)> {
    let os = HostOs::of(host);
    // alloc: setup: once per cargo started, the environment it begins with.
    let mut env = started_with.to_vec();
    if os.windows || os.macos {
        return env;
    }
    let existing = |name: &str| {
        started_with
            .iter()
            .find(|(it, _)| it == OsStr::new(name))
            .map(|(_, value)| PathBuf::from(value))
            .filter(|it| it.exists())
    };
    let mut cert_file = existing("SSL_CERT_FILE");
    let mut cert_dir = existing("SSL_CERT_DIR");
    // Each candidate is joined in this one buffer, and copied out only when it is found.
    let mut candidate = PathBuf::new();
    for dir in certificate_dirs.iter().filter(|it| it.exists()) {
        if cert_file.is_none() {
            cert_file = CERTIFICATE_FILES.iter().find_map(|it| {
                let path = joined(&mut candidate, dir, it);
                // alloc: setup: once per cargo started, a certificate path found.
                path.exists().then(|| path.to_owned())
            });
        }
        if cert_dir.is_none() {
            let path = joined(&mut candidate, dir, "certs");
            if path.exists() {
                // alloc: setup: once per cargo started, a certificate path found.
                cert_dir = Some(path.to_owned());
            }
        }
        if cert_file.is_some() && cert_dir.is_some() {
            break;
        }
    }
    for (name, value) in [("SSL_CERT_FILE", cert_file), ("SSL_CERT_DIR", cert_dir)] {
        let Some(value) = value else { continue };
        match env.iter_mut().find(|(it, _)| it == OsStr::new(name)) {
            Some((_, it)) => *it = value.into_os_string(),
            None => env.push((name.into(), value.into_os_string())),
        }
    }
    env
}

/// `dir` and `name` joined in `buf`, which keeps its allocation from one join to the next.
fn joined<'b>(buf: &'b mut PathBuf, dir: &Path, name: &str) -> &'b Path {
    buf.as_mut_os_string().clear();
    buf.push(dir);
    buf.push(name);
    buf
}

/// About how many words cargo has on the fixed set of names, the dynamic library path's two
/// among them.
const FIXED: usize = 40;

/// The environment cargo runs `unit`'s script with, when cargo's own is `inherited` (see
/// [`cargo_started`]).
pub fn env(unit: &Unit, inherited: &[(OsString, OsString)]) -> Result<Env, Error> {
    unit.check()?;
    let host = HostOs::of(&unit.host);
    let inherited_var = |name: &str| {
        inherited.iter().find(|(it, _)| it == OsStr::new(name)).map(|(_, value)| value.as_os_str())
    };
    let package = &unit.package;
    let metadata: usize = unit.dependencies.iter().map(|it| it.metadata.len()).sum();
    let mut cmd = Explicit {
        vars: Vec::with_capacity(
            FIXED
                + unit.config_env.len()
                + unit.features.len()
                + unit.target_cfgs.len()
                + metadata
                + 1,
        ),
    };

    // `fill_env`, for a process on the host.
    let search = &unit.dylib_search;
    let mut search_path = Vec::with_capacity(search.host_deps.len() + 5);
    if let Some(host_root) = &search.host_root {
        // alloc: setup, once per unit: the search path it sets.
        search_path.push(host_root.clone());
    }
    search_path.extend(search.host_deps.iter().cloned());
    if !search.sysroot_libdir.as_os_str().is_empty() {
        // alloc: setup, once per unit: the search path it sets.
        search_path.push(search.sysroot_libdir.clone());
    }
    let dylib_path =
        inherited_var(host.dylib_var).map(|it| host.split_paths(it)).unwrap_or_default();
    let dylib_path_is_empty = dylib_path.is_empty();
    if dylib_path.starts_with(&search_path) {
        search_path = dylib_path;
    } else {
        search_path.extend(dylib_path);
    }
    if host.macos && dylib_path_is_empty {
        if let Some(home) = inherited_var("HOME") {
            search_path.push(Path::new(home).join("lib"));
        }
        search_path.push(PathBuf::from("/usr/local/lib"));
        search_path.push(PathBuf::from("/usr/lib"));
    }
    cmd.set(host.dylib_var, host.join_paths(&search_path, host.dylib_var)?);
    cmd.set("CARGO", &unit.tools.cargo);
    let version = Version::of(package)?;
    cmd.set("CARGO_MANIFEST_DIR", &package.manifest_dir);
    cmd.set("CARGO_MANIFEST_PATH", &package.manifest_path);
    cmd.set("CARGO_PKG_VERSION_MAJOR", version.major);
    cmd.set("CARGO_PKG_VERSION_MINOR", version.minor);
    cmd.set("CARGO_PKG_VERSION_PATCH", version.patch);
    cmd.set("CARGO_PKG_VERSION_PRE", version.pre);
    cmd.set("CARGO_PKG_VERSION", &package.version);
    cmd.set("CARGO_PKG_NAME", &package.name);
    fn field(it: &Option<String>) -> &str {
        it.as_deref().unwrap_or("")
    }
    cmd.set("CARGO_PKG_DESCRIPTION", field(&package.description));
    cmd.set("CARGO_PKG_HOMEPAGE", field(&package.homepage));
    cmd.set("CARGO_PKG_REPOSITORY", field(&package.repository));
    cmd.set("CARGO_PKG_LICENSE", field(&package.license));
    cmd.set("CARGO_PKG_LICENSE_FILE", field(&package.license_file));
    cmd.set("CARGO_PKG_AUTHORS", package.authors.join(":"));
    cmd.set("CARGO_PKG_RUST_VERSION", field(&package.rust_version));
    cmd.set("CARGO_PKG_README", field(&package.readme));
    // `[env]` takes nothing cargo has set, and only what cargo's environment lacks unless forced.
    for it in &unit.config_env {
        if (it.force || inherited_var(&it.name).is_none()) && !cmd.contains(&it.name) {
            // clone: setup, once per unit: an `[env]` name it sets.
            cmd.set(it.name.clone(), &it.value);
        }
    }

    // `build_work`.
    cmd.set("OUT_DIR", &unit.out_dir);
    cmd.set("CARGO_MANIFEST_DIR", &package.manifest_dir);
    cmd.set("CARGO_MANIFEST_PATH", &package.manifest_path);
    cmd.set("NUM_JOBS", decimal(unit.jobs));
    cmd.set("TARGET", &unit.target);
    cmd.set("DEBUG", if unit.profile.debuginfo { "true" } else { "false" });
    cmd.set("OPT_LEVEL", &unit.profile.opt_level);
    cmd.set("PROFILE", if unit.profile.release { "release" } else { "debug" });
    cmd.set("HOST", &unit.host);
    cmd.set("RUSTC", &unit.tools.rustc);
    cmd.set("RUSTDOC", &unit.tools.rustdoc);
    if let Some(linker) = &unit.tools.linker {
        cmd.set("RUSTC_LINKER", linker);
    }
    if let Some(links) = &package.links {
        cmd.set("CARGO_MANIFEST_LINKS", links);
    }
    for feature in &unit.features {
        cmd.set(prefixed("CARGO_FEATURE_", feature), "1");
    }
    // The cfgs as cargo's map has them: `feature` first, `debug_assertions` from the profile
    // and not from rustc, which asks without the profile's flags; a name again empties it.
    let mut cfgs = Cfgs::default();
    cfgs.insert("feature", unit.features.iter().map(String::as_str));
    if unit.profile.debug_assertions {
        cfgs.insert("debug_assertions", []);
    }
    for line in &unit.target_cfgs {
        match cfg_of(package, line)? {
            ("debug_assertions", None) => {}
            (name, None) => cfgs.insert(name, []),
            (name, Some(value)) => cfgs.add(name, value),
        }
    }
    for (name, values) in cfgs.joined() {
        cmd.set(prefixed("CARGO_CFG_", name), values);
    }
    match &unit.tools.rustc_wrapper {
        Some(wrapper) => cmd.set("RUSTC_WRAPPER", wrapper),
        None => cmd.remove("RUSTC_WRAPPER"),
    }
    cmd.remove("RUSTC_WORKSPACE_WRAPPER");
    if unit.workspace_member
        && let Some(wrapper) = &unit.tools.rustc_workspace_wrapper
    {
        cmd.set("RUSTC_WORKSPACE_WRAPPER", wrapper);
    }
    cmd.set("CARGO_ENCODED_RUSTFLAGS", unit.rustflags.join("\x1f"));
    cmd.remove("RUSTFLAGS");
    for dependency in &unit.dependencies {
        let Some(links) = &dependency.links else { continue };
        let mut prefix = prefixed("DEP_", links);
        prefix.push('_');
        for (key, value) in &dependency.metadata {
            cmd.set(prefixed(&prefix, key), value);
        }
    }
    // `add_plugin_deps`.
    if let Some(host_root) = &search.host_root {
        let mut search_path =
            cmd.get(host.dylib_var).map(|it| host.split_paths(it)).unwrap_or_default();
        search_path.extend(below(&search.plugin_search, host_root));
        cmd.set(host.dylib_var, host.join_paths(&search_path, host.dylib_var)?);
    }
    // The jobserver's `Client::configure`.
    if let Some(auth) = &unit.jobserver {
        cmd.set(
            "CARGO_MAKEFLAGS",
            ["-j --jobserver-fds=", auth.as_str(), " --jobserver-auth=", auth.as_str()].concat(),
        );
    }
    Ok(merged(inherited, cmd))
}

/// A variable cargo sets (or takes away), before it is sorted.
struct Entry {
    /// The name's first sixteen bytes, big-endian: most comparisons of names end here. Eight
    /// would tie on most of them (`CARGO_CF`, `CARGO_FE`, `CARGO_PK`); sixteen part all but
    /// the target's cfgs, at one compare of two words.
    order: u128,
    name: OsString,
    value: Option<OsString>,
    from_unit: bool,
    /// Which word on its name it is: a later one wins.
    seq: u32,
}

impl Entry {
    fn new(name: OsString, value: Option<OsString>, from_unit: bool, seq: usize) -> Self {
        let bytes = name.as_encoded_bytes();
        let mut first = [0u8; 16];
        let len = bytes.len().min(16);
        first[..len].copy_from_slice(&bytes[..len]);
        Entry { order: u128::from_be_bytes(first), name, value, from_unit, seq: seq as u32 }
    }
}

/// What cargo inherits with what it sets over it, sorted by name, the last word on each name
/// kept and what is taken away left out. What cargo sets is sorted; what is inherited is
/// the same for every script of a build, so a caller that passes it sorted (each name once)
/// pays one pass to check that, not a sort; otherwise its indices are sorted. The two are
/// then merged, what cargo sets winning, and an inherited variable is copied only where kept.
fn merged(inherited: &[(OsString, OsString)], cmd: Explicit) -> Env {
    let mut own: Vec<Entry> = Vec::with_capacity(cmd.vars.len());
    for (seq, (name, value)) in cmd.vars.into_iter().enumerate() {
        let from_unit = value.is_some();
        own.push(Entry::new(OsString::from(name.into_owned()), value, from_unit, seq));
    }
    own.sort_unstable_by(|a, b| {
        a.order.cmp(&b.order).then_with(|| a.name.cmp(&b.name)).then(b.seq.cmp(&a.seq))
    });
    own.dedup_by(|later, first| later.name == first.name);

    let order: Option<Vec<u32>> = (!inherited.is_sorted_by(|a, b| a.0 < b.0)).then(|| {
        // alloc: setup, once per unit: the inherited order, when not passed sorted (request 6).
        let mut order: Vec<u32> = (0..inherited.len() as u32).collect();
        order.sort_unstable_by(|a, b| {
            inherited[*a as usize].0.cmp(&inherited[*b as usize].0).then(b.cmp(a))
        });
        order.dedup_by(|later, first| inherited[*later as usize].0 == inherited[*first as usize].0);
        order
    });
    let kept = order.as_ref().map_or(inherited.len(), Vec::len);
    let inherited_at = |at: usize| match &order {
        Some(order) => order.get(at).map(|it| &inherited[*it as usize]),
        None => inherited.get(at),
    };

    let mut vars = Vec::with_capacity(kept + own.len());
    let mut from_unit = Vec::with_capacity(kept + own.len());
    let mut own = own.into_iter().peekable();
    let mut at = 0;
    loop {
        let next_inherited = inherited_at(at);
        let next = match (own.peek(), next_inherited) {
            (None, None) => break,
            (Some(set), Some((name, _))) => set.name.cmp(name),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
        };
        if next != Ordering::Less
            && let Some((name, value)) = next_inherited
        {
            at += 1;
            if next == Ordering::Greater {
                // alloc: setup, once per unit: an inherited variable copied where it is kept.
                vars.push((name.clone(), value.clone()));
                from_unit.push(false);
                continue;
            }
        }
        if let Some(Entry { name, value: Some(value), from_unit: set_from_unit, .. }) = own.next() {
            vars.push((name, value));
            from_unit.push(set_from_unit);
        }
    }
    Env { vars, from_unit }
}

/// The parts of a version cargo names apart.
struct Version<'a> {
    major: &'a str,
    minor: &'a str,
    patch: &'a str,
    pre: &'a str,
}

impl Version<'_> {
    /// The parts of `package`'s version, which cargo's manifest reader has made semver's: three
    /// numbers, then a pre-release and build metadata, each optional.
    fn of(package: &Package) -> Result<Version<'_>, Unfilled> {
        let whole = package.version.as_str();
        let version = whole.split_once('+').map_or(whole, |(it, _)| it);
        let (numbers, pre) = version.split_once('-').unwrap_or((version, ""));
        let digits = |it: &str| !it.is_empty() && it.bytes().all(|b| b.is_ascii_digit());
        let mut parts = numbers.split('.');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(major), Some(minor), Some(patch), None)
                if digits(major) && digits(minor) && digits(patch) =>
            {
                Ok(Version { major, minor, patch, pre })
            }
            // alloc: error path.
            _ => Err(Unfilled::Version { package: package.descr(), version: whole.to_owned() }),
        }
    }
}

#[cfg(test)]
mod tests;
