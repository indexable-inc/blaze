//! clippy's configuration of a package: its `clippy.toml` (or `.clippy.toml`), found as
//! clippy finds it, and the Rust version it is to build on, which some lints keep to.
//!
//! Follows clippy's `clippy_config/src/conf.rs` (`load_conf_file`, `Conf::load_inner`) at
//! rust-lang/rust 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A Rust version, as `rust-version` and `msrv` give it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// `1.70` or `1.70.0`, as clippy's `parse_version` reads it.
    pub fn parse(text: &str) -> Option<Version> {
        let mut parts = text.trim().split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().map_or(Some(0), |it| it.parse().ok())?;
        let patch = parts.next().map_or(Some(0), |it| it.parse().ok())?;
        parts.next().is_none().then_some(Version(major, minor, patch))
    }
}

/// A package's clippy configuration. A key of the lint queries ([`crate::query::LintSet`]), so
/// compared and hashed by what it is.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Conf {
    /// The file it was read from.
    pub path: Option<PathBuf>,
    /// Its keys, as written.
    #[serde(default)]
    pub table: toml::Table,
    /// `msrv`, else the package's `rust-version`.
    pub msrv: Option<Version>,
}

/// A `clippy.toml` holds no NaN that two equal configurations could differ by in practice; a
/// NaN only makes two loads of one file two keys, never two files one.
impl Eq for Conf {}

impl std::hash::Hash for Conf {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.msrv.hash(state);
        state.write_u64(table_hash(&self.table));
    }
}

/// A hash of `table` that its order does not change, as its equality doesn't.
fn table_hash(table: &toml::Table) -> u64 {
    table.iter().fold(table.len() as u64, |sum, (key, value)| {
        let mut entry = blaze_hash::FixedHasher::default();
        std::hash::Hash::hash(key, &mut entry);
        std::hash::Hasher::write_u64(&mut entry, value_hash(value));
        sum.wrapping_add(std::hash::Hasher::finish(&entry))
    })
}

fn value_hash(value: &toml::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut state = blaze_hash::FixedHasher::default();
    match value {
        toml::Value::String(it) => (0u8, it).hash(&mut state),
        toml::Value::Integer(it) => (1u8, it).hash(&mut state),
        toml::Value::Float(it) => (2u8, it.to_bits()).hash(&mut state),
        toml::Value::Boolean(it) => (3u8, it).hash(&mut state),
        toml::Value::Datetime(it) => (4u8, it.to_string()).hash(&mut state),
        toml::Value::Array(items) => {
            5u8.hash(&mut state);
            items.iter().for_each(|it| state.write_u64(value_hash(it)));
        }
        toml::Value::Table(it) => (6u8, table_hash(it)).hash(&mut state),
    }
    state.finish()
}

/// The counts the lints read ([`Conf::int`]): `u64` in clippy's `Conf`, and blaze's own.
const COUNTS: [&str; 9] = [
    "array-size-threshold",
    "stack-size-threshold",
    "enum-variant-size-threshold",
    "pass-by-value-size-limit",
    "too-large-for-stack",
    "type-complexity-threshold",
    "vec-box-size-threshold",
    "parallel-threshold",
    "parallel-assumed-trips",
];

/// The flags the lints read ([`Conf::bool`]).
const FLAGS: [&str; 7] = [
    "accept-comment-above-attributes",
    "accept-comment-above-statement",
    "allow-dbg-in-tests",
    "allow-print-in-tests",
    "allow-useless-vec-in-tests",
    "avoid-breaking-exported-api",
    "upper-case-acronyms-aggressive",
];

/// A value of a key the lints read that is not of its type (or a negative count) is clippy's
/// error as it loads the file, never the key's default in its place.
fn checked(table: &toml::Table) -> Result<(), String> {
    for key in COUNTS {
        match table.get(key) {
            None => {}
            Some(toml::Value::Integer(it)) if *it >= 0 => {}
            Some(toml::Value::Integer(it)) => {
                return Err(format!(
                    "error reading Clippy's configuration file: invalid value: integer `{it}`, expected u64 for `{key}`"
                ));
            }
            Some(other) => {
                return Err(format!(
                    "error reading Clippy's configuration file: invalid type: {}, expected u64 for `{key}`",
                    other.type_str()
                ));
            }
        }
    }
    for key in FLAGS {
        match table.get(key) {
            None | Some(toml::Value::Boolean(_)) => {}
            Some(other) => {
                return Err(format!(
                    "error reading Clippy's configuration file: invalid type: {}, expected a boolean for `{key}`",
                    other.type_str()
                ));
            }
        }
    }
    for kind in Kind::ALL {
        disallowed(table, kind)?;
    }
    Ok(())
}

/// Which list of disallowed items an entry is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Types,
    Methods,
    Macros,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Types, Kind::Methods, Kind::Macros];

    /// Its key in `clippy.toml`.
    pub const fn key(self) -> &'static str {
        match self {
            Kind::Types => "disallowed-types",
            Kind::Methods => "disallowed-methods",
            Kind::Macros => "disallowed-macros",
        }
    }
}

/// An entry of a `disallowed-*` list: clippy's `DisallowedPath`, written as its path alone or
/// as a table, with blaze's `allow-in` and `lint` beside clippy's keys.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Disallowed {
    /// The item, as a path from a crate's root: `std::sync::Mutex`, `std::time::Instant::now`.
    pub path: String,
    pub reason: Option<String>,
    /// The item to use instead, as a path: the fix rewrites to it.
    pub replacement: Option<String>,
    /// Whether a path that resolves to nothing is no error (an item of a crate the package
    /// may not depend on).
    pub allow_invalid: bool,
    /// The modules where the item is allowed, as paths from the crate's root (`crate::driver`),
    /// each with the modules inside it.
    pub allow_in: Vec<String>,
    /// The lint a use of it is reported under, when not clippy's `disallowed_*` for its list:
    /// `blaze::wall_clock`, which an `#[expect]` can name.
    pub lint: Option<String>,
}

const ENTRY_KEYS: [&str; 6] = ["path", "reason", "replacement", "allow-invalid", "allow-in", "lint"];

/// The entries of `table`'s list `kind`; an entry that does not read is the error, naming it.
pub fn disallowed(table: &toml::Table, kind: Kind) -> Result<Vec<Disallowed>, String> {
    let key = kind.key();
    let Some(list) = table.get(key) else { return Ok(Vec::new()) };
    let error = |what: String| format!("error reading Clippy's configuration file: {what} in `{key}`");
    let list = list.as_array().ok_or_else(|| error(format!("invalid type: {}, expected a list", list.type_str())))?;
    let mut entries = Vec::with_capacity(list.len());
    for item in list {
        let entry = match item {
            toml::Value::String(path) => Disallowed { path: path.clone(), ..Disallowed::default() },
            toml::Value::Table(fields) => {
                if let Some(unknown) = fields.keys().find(|it| !ENTRY_KEYS.contains(&it.as_str())) {
                    return Err(error(format!("unknown field `{unknown}`, expected one of {}", ENTRY_KEYS.join(", "))));
                }
                let string = |name: &str| match fields.get(name) {
                    None => Ok(None),
                    Some(toml::Value::String(it)) => Ok(Some(it.clone())),
                    Some(other) => Err(error(format!("invalid type: {}, expected a string for `{name}`", other.type_str()))),
                };
                let path = string("path")?.ok_or_else(|| error("an entry with no `path`".to_owned()))?;
                let allow_invalid = match fields.get("allow-invalid") {
                    None => false,
                    Some(toml::Value::Boolean(it)) => *it,
                    Some(other) => {
                        return Err(error(format!("invalid type: {}, expected a boolean for `allow-invalid` of `{path}`", other.type_str())));
                    }
                };
                let allow_in = match fields.get("allow-in") {
                    None => Vec::new(),
                    Some(toml::Value::Array(items)) => items
                        .iter()
                        .map(|it| {
                            it.as_str()
                                .filter(|it| *it == "crate" || it.starts_with("crate::"))
                                .map(str::to_owned)
                                .ok_or_else(|| error(format!("`allow-in` of `{path}` has {it}, not a module path from `crate`")))
                        })
                        .collect::<Result<_, _>>()?,
                    Some(other) => {
                        return Err(error(format!("invalid type: {}, expected a list for `allow-in` of `{path}`", other.type_str())));
                    }
                };
                let lint = string("lint")?;
                if let Some(lint) = &lint
                    && !lint.starts_with("blaze::")
                {
                    return Err(error(format!("`lint` of `{path}` is `{lint}`, not a lint of blaze's")));
                }
                Disallowed { reason: string("reason")?, replacement: string("replacement")?, allow_invalid, allow_in, lint, path }
            }
            other => return Err(error(format!("invalid type: {}, expected a path or a table", other.type_str()))),
        };
        entries.push(entry);
    }
    Ok(entries)
}

/// The names clippy looks for, in the order it takes them.
const NAMES: [&str; 2] = [".clippy.toml", "clippy.toml"];

/// The configuration file clippy reads for a package: in `CLIPPY_CONF_DIR` when that is set,
/// else in the package's directory, else in the nearest directory above with one.
pub fn find(conf_dir: Option<&Path>, package_dir: &Path) -> Option<PathBuf> {
    let mut current = conf_dir.unwrap_or(package_dir).canonicalize().ok()?;
    loop {
        if let Some(found) = NAMES.iter().map(|it| current.join(it)).find(|it| it.is_file()) {
            return Some(found);
        }
        if !current.pop() {
            return None;
        }
    }
}

impl Conf {
    /// The configuration of a package whose file is `path`, if it has one, and whose
    /// `rust-version` is `rust_version`.
    pub fn load(path: Option<PathBuf>, rust_version: Option<&str>) -> Result<Conf, String> {
        let file = match path {
            Some(path) => {
                let text = std::fs::read_to_string(&path)
                    .map_err(|error| format!("error reading `{}`: {error}", path.display()))?;
                Some((path, text))
            }
            None => None,
        };
        Conf::of_text(file, rust_version)
    }

    /// [`Conf::load`] of a file already read: its path and its text.
    pub fn of_text(file: Option<(PathBuf, String)>, rust_version: Option<&str>) -> Result<Conf, String> {
        let (path, table) = match file {
            Some((path, text)) => {
                let table = text
                    .parse::<toml::Table>()
                    .map_err(|error| format!("error parsing `clippy.toml`: {}", error.message()))?;
                (Some(path), table)
            }
            None => (None, toml::Table::new()),
        };
        checked(&table)?;
        let msrv = match table.get("msrv").or_else(|| table.get("min-rust-version")) {
            Some(toml::Value::String(it)) => Some(Version::parse(it).ok_or_else(|| {
                format!("error reading Clippy's configuration file: `{it}` is not a valid Rust version")
            })?),
            Some(other) => {
                return Err(format!(
                    "error reading Clippy's configuration file: invalid type: {}, expected a version for `msrv`",
                    other.type_str()
                ));
            }
            None => rust_version.and_then(Version::parse),
        };
        Ok(Conf { path, table, msrv })
    }

    pub fn bool(&self, key: &str, default: bool) -> bool {
        self.table.get(key).and_then(toml::Value::as_bool).unwrap_or(default)
    }

    pub fn int(&self, key: &str, default: i64) -> i64 {
        self.table.get(key).and_then(toml::Value::as_integer).unwrap_or(default)
    }

    pub fn strings(&self, key: &str) -> Option<Vec<String>> {
        let list = self.table.get(key)?.as_array()?;
        Some(list.iter().filter_map(|it| it.as_str().map(str::to_owned)).collect())
    }

    pub fn value(&self, key: &str) -> Option<&toml::Value> {
        self.table.get(key)
    }

    /// The entries of the list `kind`.
    pub fn disallowed(&self, kind: Kind) -> Result<Vec<Disallowed>, String> {
        disallowed(&self.table, kind)
    }

    /// Whether the Rust version kept to has what came in `version`: clippy's `Msrv::meets`,
    /// which with no version set keeps to none.
    pub fn meets(&self, version: Version) -> bool {
        self.msrv.is_none_or(|it| it >= version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_as_clippy_reads_them() {
        assert_eq!(Version::parse("1.70"), Some(Version(1, 70, 0)));
        assert_eq!(Version::parse("1.74.1"), Some(Version(1, 74, 1)));
        assert_eq!(Version::parse("one"), None);
    }

    #[test]
    fn msrv_from_the_file_before_the_manifest() {
        let dir = std::env::temp_dir().join(format!("blaze-lints-conf-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("clippy.toml"), "msrv = \"1.60\"\nallow-dbg-in-tests = true\n")
            .unwrap();
        let found = find(None, &dir.join("sub")).unwrap();
        let conf = Conf::load(Some(found), Some("1.80")).unwrap();
        assert_eq!(conf.msrv, Some(Version(1, 60, 0)));
        assert!(conf.bool("allow-dbg-in-tests", false));
        assert!(!conf.meets(Version(1, 70, 0)));
        let conf = Conf::load(None, Some("1.80")).unwrap();
        assert!(conf.meets(Version(1, 70, 0)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A value of the wrong type, a negative count or a version that does not read is the
    /// load's error, never the default.
    #[test]
    fn a_value_of_the_wrong_type_is_an_error() {
        let dir = std::env::temp_dir().join(format!("blaze-lints-conf-types-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clippy.toml");
        for (text, says) in [
            ("too-large-for-stack = -1\n", "integer `-1`"),
            ("vec-box-size-threshold = \"big\"\n", "expected u64 for `vec-box-size-threshold`"),
            ("allow-dbg-in-tests = 1\n", "expected a boolean for `allow-dbg-in-tests`"),
            ("msrv = \"one\"\n", "`one` is not a valid Rust version"),
        ] {
            std::fs::write(&path, text).unwrap();
            let error = Conf::load(Some(path.clone()), None).expect_err(text);
            assert!(error.contains(says), "{text}: {error}");
        }
        std::fs::write(&path, "too-large-for-stack = 10\nallow-dbg-in-tests = true\n").unwrap();
        assert_eq!(Conf::load(Some(path), None).unwrap().int("too-large-for-stack", 200), 10);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn disallowed_entries_read_as_a_path_or_a_table_and_refuse_what_clippy_and_blaze_do_not_know() {
        let table: toml::Table = r#"
disallowed-types = [
    "std::collections::LinkedList",
    { path = "std::sync::Mutex", replacement = "parking_lot::Mutex", reason = "no poisoning" },
    { path = "dashmap::DashMap", allow-invalid = true },
]
disallowed-methods = [
    { path = "std::time::Instant::now", allow-in = ["crate::driver"], lint = "blaze::wall_clock" },
]
"#
        .parse()
        .unwrap();
        checked(&table).unwrap();
        let types = disallowed(&table, Kind::Types).unwrap();
        assert_eq!(types[0], Disallowed { path: "std::collections::LinkedList".to_owned(), ..Disallowed::default() });
        assert_eq!(types[1].replacement.as_deref(), Some("parking_lot::Mutex"));
        assert_eq!(types[1].reason.as_deref(), Some("no poisoning"));
        assert!(types[2].allow_invalid);
        let methods = disallowed(&table, Kind::Methods).unwrap();
        assert_eq!(methods[0].allow_in, ["crate::driver"]);
        assert_eq!(methods[0].lint.as_deref(), Some("blaze::wall_clock"));
        assert!(disallowed(&table, Kind::Macros).unwrap().is_empty());
        for (text, says) in [
            ("disallowed-types = [{ path = \"a::B\", replace = \"c::D\" }]\n", "unknown field `replace`"),
            ("disallowed-types = [{ reason = \"x\" }]\n", "an entry with no `path`"),
            ("disallowed-methods = [{ path = \"a::b\", allow-in = [\"driver\"] }]\n", "not a module path from `crate`"),
            ("disallowed-macros = [{ path = \"a::b\", lint = \"clippy::x\" }]\n", "not a lint of blaze's"),
            ("disallowed-types = \"a::B\"\n", "expected a list"),
        ] {
            let error = checked(&text.parse().unwrap()).expect_err(text);
            assert!(error.contains(says), "{text}: {error}");
        }
    }

    #[test]
    fn equal_confs_hash_alike_in_any_order() {
        use std::hash::BuildHasher as _;
        let of = |text: &str| Conf { table: text.parse().unwrap(), ..Conf::default() };
        let (a, b) = (of("a = 1\nb = [true, 2.5]\n"), of("b = [true, 2.5]\na = 1\n"));
        assert_eq!(a, b);
        let hash = |it: &Conf| blaze_hash::FixedState.hash_one(it);
        assert_eq!(hash(&a), hash(&b));
        assert_ne!(hash(&a), hash(&of("a = 2\nb = [true, 2.5]\n")));
    }
}
