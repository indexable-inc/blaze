//! cargo's extra-flags rule for a unit's rustc, as far as a unit's cfg is concerned (the Cargo
//! Book, "Configuration", `build.rustflags`): "There are four mutually exclusive sources of
//! extra flags. They are checked in order, with the first one being used":
//! `CARGO_ENCODED_RUSTFLAGS`, `RUSTFLAGS`, all matching `target.<triple>.rustflags` and
//! `target.<cfg>.rustflags` joined, `build.rustflags`. "If `--target` is passed on the
//! command-line, then the flags will only be passed to the compiler for the target. Things
//! being built for the host, such as build scripts or proc macros, will not receive the args.
//! Without `--target`, the flags will be passed to all compiler invocations."
//!
//! Sans-IO over text (rule 14, rule 75): the caller reads the config files (blaze-cli graph.rs
//! `rustflags_config`, each a row of the units' source, so editing one re-lays the graph) and
//! hands the environment's variables read at its edge. [`choose`] answers the flags' `--cfg`
//! atoms, which go into the unit's cfg and so its key (rule 21), and every other flag, which
//! blaze does not apply, by name ([`Chosen::not_applied`]: never dropped quietly, rule 3).
//!
//! A `target.'cfg(..)'` table is matched against the target's cfg as cargo's `TargetInfo::new`
//! does: first without the cfg tables, then again with the `--cfg`s the first answer holds added
//! to the target's atoms (rustc prints them in `--print cfg`), at most twice
//! ([`Chosen::settled`] says whether the second turn agreed).
//!
//! The one `--cfg` parser ([`cfg_spec`]) reads a spec by the cfg grammar (blaze-cond `cfg`):
//! `name` or `name = "value"`, nothing else.
//!
//! Not read yet, each named where it matters: config files above the checkout and
//! `$CARGO_HOME/config.toml` (the caller reads the checkout's own); `CARGO_BUILD_RUSTFLAGS` and
//! `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` (cargo merges them into the files' values: refused by name,
//! [`RustflagsError::NotMerged`]); an `include` inside an included file
//! ([`RustflagsError::NestedInclude`]).

use core::fmt;

use blaze_own::{Alloc, String, Vec};
use blaze_toml::{Document, Kind, Limits};

pub type Text<'a> = String<Alloc<'a>>;

/// The variables of cargo's rule, each read at the command's edge (rule 75), `None` when unset.
#[derive(Clone, Copy, Default, Debug)]
pub struct RustflagsEnv<'e> {
    /// `CARGO_ENCODED_RUSTFLAGS`: flags split on `\x1f`; empty is no flags.
    pub encoded: Option<&'e str>,
    /// `RUSTFLAGS`: flags split on spaces.
    pub plain: Option<&'e str>,
    /// `CARGO_BUILD_RUSTFLAGS`, which cargo merges into `build.rustflags`.
    pub build: Option<&'e str>,
    /// `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` ([`target_variable`]), merged into the triple's table.
    pub target: Option<&'e str>,
}

/// The variable cargo reads for `target.<triple>.rustflags`: `CARGO_TARGET_`, the triple
/// upper-cased with `-` and `.` as `_`, then `_RUSTFLAGS`.
pub fn target_variable<'a>(triple: &str, alloc: Alloc<'a>) -> Text<'a> {
    let mut out = String::from_str_in("CARGO_TARGET_", alloc);
    for c in triple.chars() {
        out.push(match c {
            '-' | '.' => '_',
            c => c.to_ascii_uppercase(),
        });
    }
    out.push_str("_RUSTFLAGS");
    out
}

/// One `rustflags` value as config files write it, merged: a string (split on whitespace) or
/// an array of strings.
struct Value<'a> {
    list: Vec<Text<'a>, Alloc<'a>>,
    string: bool,
}

/// The `rustflags` of a workspace's config files, merged in precedence order.
pub struct RustflagsConfig<'a> {
    build: Option<Value<'a>>,
    /// `target.<key>.rustflags` by key (a triple or `cfg(..)`), sorted by key as cargo's
    /// `BTreeMap` of the tables is.
    tables: Vec<(Text<'a>, Value<'a>), Alloc<'a>>,
    alloc: Alloc<'a>,
}

/// An `include` entry of a config file: the path as written (relative to the file's folder)
/// and whether it is optional.
pub struct Include<'a> {
    pub path: Text<'a>,
    pub optional: bool,
}

impl<'a> RustflagsConfig<'a> {
    /// No file read: every source but the environment is unset.
    pub fn new(alloc: Alloc<'a>) -> RustflagsConfig<'a> {
        RustflagsConfig { build: None, tables: Vec::new_in(alloc), alloc }
    }

    /// The `include` entries of the file `file` (`text`), in order: what the caller reads and
    /// merges before the file itself (an included file is of lower precedence). An entry is a
    /// string, or a table with `path` and `optional`.
    pub fn includes(file: &str, text: &str, alloc: Alloc<'a>) -> Result<Vec<Include<'a>, Alloc<'a>>, RustflagsError<'a>> {
        let doc = Document::parse_in(text, Limits::CARGO, alloc).map_err(|error| RustflagsError::Toml { file: String::from_str_in(file, alloc), error })?;
        let mut out = Vec::new_in(alloc);
        let Some(include) = doc.root().get("include") else { return Ok(out) };
        match include.kind() {
            Kind::Array => {
                for entry in include.children() {
                    out.push(include_entry(file, entry, alloc)?);
                }
            }
            _ => out.push(include_entry(file, include, alloc)?),
        }
        Ok(out)
    }

    /// The file `file` (`text`) merged over what was read before it (it is of higher
    /// precedence): an array joins after the earlier one, a string replaces it, and an array
    /// over a string (or the reverse) is refused, as cargo's merge refuses it.
    pub fn read(&mut self, file: &str, text: &str) -> Result<(), RustflagsError<'a>> {
        let alloc = self.alloc;
        let doc = Document::parse_in(text, Limits::CARGO, alloc).map_err(|error| RustflagsError::Toml { file: String::from_str_in(file, alloc), error })?;
        let root = doc.root();
        if let Some(flags) = root.get("build").and_then(|build| build.get("rustflags")) {
            let value = value_of(file, "build.rustflags", flags, alloc)?;
            merge(&mut self.build, value, "build.rustflags", alloc)?;
        }
        if let Some(target) = root.get("target") {
            for table in target.children() {
                let Some(flags) = table.get("rustflags") else { continue };
                let key = table.key();
                let value = value_of(file, key, flags, alloc)?;
                match self.tables.binary_search_by(|(k, _)| k.as_str().cmp(key)) {
                    Ok(at) => merge_into(&mut self.tables[at].1, value, key, alloc)?,
                    Err(at) => self.tables.insert(at, (String::from_str_in(key, alloc), value)),
                }
            }
        }
        Ok(())
    }
}

/// One `include` entry: a path string, or a table with a `path` string and an `optional` bool.
fn include_entry<'a>(file: &str, entry: blaze_toml::Item<'_, '_, Alloc<'a>>, alloc: Alloc<'a>) -> Result<Include<'a>, RustflagsError<'a>> {
    let bad = |at: usize| RustflagsError::Include { file: String::from_str_in(file, alloc), at };
    if let Some(path) = entry.as_str() {
        return Ok(Include { path: String::from_str_in(path, alloc), optional: false });
    }
    let Some(path) = entry.get("path").and_then(|p| p.as_str()) else { return Err(bad(entry.span().start)) };
    let optional = match entry.get("optional") {
        None => false,
        Some(flag) => flag.as_bool().ok_or_else(|| bad(flag.span().start))?,
    };
    Ok(Include { path: String::from_str_in(path, alloc), optional })
}

/// A `rustflags` item: a string split on whitespace, or an array of strings.
fn value_of<'a>(file: &str, key: &str, item: blaze_toml::Item<'_, '_, Alloc<'a>>, alloc: Alloc<'a>) -> Result<Value<'a>, RustflagsError<'a>> {
    let not = || RustflagsError::NotFlags { file: String::from_str_in(file, alloc), key: String::from_str_in(key, alloc), at: item.span().start };
    let mut list = Vec::new_in(alloc);
    if let Some(text) = item.as_str() {
        list.extend(text.split_whitespace().map(|flag| String::from_str_in(flag, alloc)));
        return Ok(Value { list, string: true });
    }
    if item.kind() != Kind::Array {
        return Err(not());
    }
    for entry in item.children() {
        list.push(String::from_str_in(entry.as_str().ok_or_else(not)?, alloc));
    }
    Ok(Value { list, string: false })
}

fn merge<'a>(old: &mut Option<Value<'a>>, new: Value<'a>, key: &str, alloc: Alloc<'a>) -> Result<(), RustflagsError<'a>> {
    match old {
        None => *old = Some(new),
        Some(was) => merge_into(was, new, key, alloc)?,
    }
    Ok(())
}

/// `new` (of higher precedence) over `was`: an array joins after, a string replaces.
fn merge_into<'a>(was: &mut Value<'a>, new: Value<'a>, key: &str, alloc: Alloc<'a>) -> Result<(), RustflagsError<'a>> {
    if was.string != new.string {
        return Err(RustflagsError::Mixed { key: String::from_str_in(key, alloc) });
    }
    match new.string {
        true => *was = new,
        false => was.list.extend(new.list),
    }
    Ok(())
}

/// Whether two flag lists are equal, flag by flag.
fn same(a: &[Text<'_>], b: &[Text<'_>]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.as_str() == y.as_str())
}

/// The one source cargo takes the flags from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlagsFrom {
    EncodedEnv,
    Env,
    /// `target.<triple>.rustflags` and every matching `target.'cfg(..)'.rustflags`, joined.
    Target,
    Build,
    /// None of the four is set.
    Unset,
    /// A host unit (a build script, a proc macro) of a build given `--target`: no flags.
    HostOfTarget,
}

impl fmt::Display for FlagsFrom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FlagsFrom::EncodedEnv => "`CARGO_ENCODED_RUSTFLAGS`",
            FlagsFrom::Env => "`RUSTFLAGS`",
            FlagsFrom::Target => "the cargo config's `target.<triple>.rustflags` and matching `target.'cfg(..)'.rustflags`",
            FlagsFrom::Build => "the cargo config's `build.rustflags`",
            FlagsFrom::Unset => "no rustflags source",
            FlagsFrom::HostOfTarget => "no source (a host unit of a `--target` build)",
        })
    }
}

/// Which unit the flags are for: whether the build was given `--target` (blaze takes none
/// today), and whether the unit is a host unit (a build script, a proc macro).
#[derive(Clone, Copy, Debug)]
pub struct Unit {
    pub target_given: bool,
    pub host_unit: bool,
}

/// What [`choose`] answers for a unit.
pub struct Chosen<'a> {
    pub from: FlagsFrom,
    /// Each `--cfg`'s atom, in the flags' order: a name and its value.
    pub cfg: Vec<(Text<'a>, Option<Text<'a>>), Alloc<'a>>,
    /// Every other flag, as written, in order: blaze does not apply it, and says so.
    pub not_applied: Vec<Text<'a>, Alloc<'a>>,
    /// Whether the second turn's flags equal the first's (cargo warns when they do not:
    /// "non-trivial mutual dependency between target-specific configuration and RUSTFLAGS").
    pub settled: bool,
}

/// The flags cargo gives a unit of target `triple`, whose `--print cfg` atoms are `atoms`, and
/// what of them blaze applies (module docs).
pub fn choose<'a>(config: &RustflagsConfig<'_>, env: RustflagsEnv<'_>, triple: &str, atoms: &[(&str, Option<&str>)], unit: Unit, alloc: Alloc<'a>) -> Result<Chosen<'a>, RustflagsError<'a>> {
    if env.build.is_some() {
        return Err(RustflagsError::NotMerged { var: "CARGO_BUILD_RUSTFLAGS" });
    }
    if env.target.is_some() {
        return Err(RustflagsError::NotMerged { var: "CARGO_TARGET_<TRIPLE>_RUSTFLAGS" });
    }
    if unit.target_given && unit.host_unit {
        return Ok(Chosen { from: FlagsFrom::HostOfTarget, cfg: Vec::new_in(alloc), not_applied: Vec::new_in(alloc), settled: true });
    }
    let (from, first) = pick(config, env, triple, None, alloc)?;
    let (_, first_cfg) = split(from, &first, alloc)?;
    let with = |cfg: &[(Text<'a>, Option<Text<'a>>)], name: &str, value: Option<&str>| {
        atoms.iter().any(|&(n, v)| n == name && v == value) || cfg.iter().any(|(n, v)| n.as_str() == name && v.as_ref().map(|v| v.as_str()) == value)
    };
    let has_first = |name: &str, value: Option<&str>| with(&first_cfg[..], name, value);
    let (from2, second) = pick(config, env, triple, Some(&has_first), alloc)?;
    let (flags, from, settled) = if same(&second, &first) {
        (first, from, true)
    } else {
        let (_, second_cfg) = split(from2, &second, alloc)?;
        let has_second = |name: &str, value: Option<&str>| with(&second_cfg[..], name, value);
        let (_, third) = pick(config, env, triple, Some(&has_second), alloc)?;
        let settled = same(&third, &second);
        (second, from2, settled)
    };
    let (not_applied, cfg) = split(from, &flags, alloc)?;
    Ok(Chosen { from, cfg, not_applied, settled })
}

type Has<'h> = &'h dyn Fn(&str, Option<&str>) -> bool;

/// The first source of the four that is set, and its flags; `target_cfg` is `None` on cargo's
/// first turn (the `cfg(..)` tables are not looked at).
fn pick<'a>(config: &RustflagsConfig<'_>, env: RustflagsEnv<'_>, triple: &str, target_cfg: Option<Has<'_>>, alloc: Alloc<'a>) -> Result<(FlagsFrom, Vec<Text<'a>, Alloc<'a>>), RustflagsError<'a>> {
    let mut out = Vec::new_in(alloc);
    if let Some(encoded) = env.encoded {
        if !encoded.is_empty() {
            out.extend(encoded.split('\x1f').map(|flag| String::from_str_in(flag, alloc)));
        }
        return Ok((FlagsFrom::EncodedEnv, out));
    }
    if let Some(plain) = env.plain {
        out.extend(plain.split(' ').map(str::trim).filter(|flag| !flag.is_empty()).map(|flag| String::from_str_in(flag, alloc)));
        return Ok((FlagsFrom::Env, out));
    }
    // cargo's `rustflags_from_target`: the triple's table first, then each matching `cfg(..)`
    // table in key order.
    if let Some((_, value)) = config.tables.iter().find(|(key, _)| key.as_str() == triple) {
        out.extend(value.list.iter().map(|flag| String::from_str_in(flag, alloc)));
    }
    if let Some(has) = target_cfg {
        for (key, value) in config.tables.iter() {
            let key = key.as_str();
            let Some(predicate) = key.strip_prefix("cfg(").and_then(|rest| rest.strip_suffix(')')) else { continue };
            let expr = blaze_cond::cfg::parse(predicate, alloc).map_err(|error| RustflagsError::Predicate { key: String::from_str_in(key, alloc), error })?;
            if expr.holds(|atom| has(atom.name, atom.value), alloc) {
                out.extend(value.list.iter().map(|flag| String::from_str_in(flag, alloc)));
            }
        }
    }
    if !out.is_empty() {
        return Ok((FlagsFrom::Target, out));
    }
    match &config.build {
        Some(build) => {
            out.extend(build.list.iter().map(|flag| String::from_str_in(flag, alloc)));
            Ok((FlagsFrom::Build, out))
        }
        None => Ok((FlagsFrom::Unset, out)),
    }
}

/// The flags split into the ones blaze does not apply and the `--cfg` atoms (`--cfg X` or
/// `--cfg=X`).
#[allow(clippy::type_complexity, reason = "the two lists a split answers, named at `Chosen`")]
fn split<'a>(from: FlagsFrom, flags: &[Text<'_>], alloc: Alloc<'a>) -> Result<(Vec<Text<'a>, Alloc<'a>>, Vec<(Text<'a>, Option<Text<'a>>), Alloc<'a>>), RustflagsError<'a>> {
    let mut other = Vec::new_in(alloc);
    let mut cfg = Vec::new_in(alloc);
    let mut at = 0;
    while at < flags.len() {
        let flag = flags[at].as_str();
        let spec = if flag == "--cfg" {
            at += 1;
            match flags.get(at) {
                Some(spec) => Some(spec.as_str()),
                None => return Err(RustflagsError::DanglingCfg { from }),
            }
        } else {
            flag.strip_prefix("--cfg=")
        };
        match spec {
            Some(spec) => cfg.push(cfg_spec(spec, alloc).map_err(|error| RustflagsError::CfgSpec { from, spec: String::from_str_in(spec, alloc), error })?),
            None => other.push(String::from_str_in(flag, alloc)),
        }
        at += 1;
    }
    Ok((other, cfg))
}

/// The one `--cfg` parser: a spec `name` or `name = "value"` (rustc's `--cfg`, read by the cfg
/// grammar), as its name and value.
pub fn cfg_spec<'a>(spec: &str, alloc: Alloc<'a>) -> Result<(Text<'a>, Option<Text<'a>>), CfgSpecError> {
    let expr = blaze_cond::cfg::parse(spec, alloc).map_err(CfgSpecError::Parse)?;
    let atom = expr.lone_atom().ok_or(CfgSpecError::NotOneAtom)?;
    Ok((String::from_str_in(atom.name, alloc), atom.value.map(|value| String::from_str_in(value, alloc))))
}

/// Why a `--cfg` spec is refused.
#[derive(Clone, Copy, Debug)]
pub enum CfgSpecError {
    /// It does not read by the cfg grammar.
    Parse(blaze_cond::ParseError),
    /// It reads as a predicate (`all(..)`, `not(..)`, `true`), not one `name` or `name = "v"`.
    NotOneAtom,
}

/// Why the rustflags could not be read; each names the file, key or variable.
#[derive(Debug)]
pub enum RustflagsError<'a> {
    Toml { file: Text<'a>, error: blaze_toml::Error },
    /// A `rustflags` that is neither a string nor an array of strings.
    NotFlags { file: Text<'a>, key: Text<'a>, at: usize },
    /// A string over an array (or the reverse) in two files.
    Mixed { key: Text<'a> },
    /// An `include` entry that is neither a path nor a table with a `path` string.
    Include { file: Text<'a>, at: usize },
    /// An included file that includes another: blaze reads one level.
    NestedInclude { file: Text<'a> },
    /// A non-optional `include` names a file that is not there.
    IncludeMissing { file: Text<'a>, path: Text<'a> },
    /// A `--cfg` spec that is not one atom.
    CfgSpec { from: FlagsFrom, spec: Text<'a>, error: CfgSpecError },
    /// A `--cfg` with nothing after it.
    DanglingCfg { from: FlagsFrom },
    /// A `target.'cfg(..)'` key whose predicate does not parse.
    Predicate { key: Text<'a>, error: blaze_cond::ParseError },
    /// A variable cargo merges into the config's values, which blaze does not merge yet.
    NotMerged { var: &'static str },
}

impl fmt::Display for RustflagsError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RustflagsError::Toml { file, error } => write!(f, "{}: {error}", file.as_str()),
            RustflagsError::NotFlags { file, key, at } => write!(f, "{} byte {at}: `{}.rustflags` is neither a string nor an array of strings", file.as_str(), key.as_str()),
            RustflagsError::Mixed { key } => write!(f, "`{}`: a string in one config file and an array in another, which cargo refuses to merge", key.as_str()),
            RustflagsError::Include { file, at } => write!(f, "{} byte {at}: an `include` entry is neither a path nor a table with a `path` string", file.as_str()),
            RustflagsError::NestedInclude { file } => write!(f, "{}: an included config file that includes another, which blaze does not read yet", file.as_str()),
            RustflagsError::IncludeMissing { file, path } => write!(f, "{}: its `include` of `{}` (not optional) names no file", file.as_str(), path.as_str()),
            RustflagsError::CfgSpec { from, spec, error } => match error {
                CfgSpecError::Parse(error) => write!(f, "the rustflags of {from}: `--cfg {}` is no cfg spec ({error}); a spec is `name` or `name=\"value\"`", spec.as_str()),
                CfgSpecError::NotOneAtom => write!(f, "the rustflags of {from}: `--cfg {}` is a predicate, not one `name` or `name=\"value\"`", spec.as_str()),
            },
            RustflagsError::DanglingCfg { from } => write!(f, "the rustflags of {from}: a `--cfg` with no spec after it"),
            RustflagsError::Predicate { key, error } => write!(f, "`target.'{}'`: its predicate does not parse: {error}", key.as_str()),
            RustflagsError::NotMerged { var } => write!(f, "`{var}` is set: cargo merges it into the config's rustflags, which blaze does not do yet, so it refuses rather than guess (rule 3)"),
        }
    }
}

#[cfg(test)]
#[path = "rustflags_tests.rs"]
mod tests;
