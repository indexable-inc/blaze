//! Real profile data for the lints: a workspace names an LLVM indexed profile (`.profdata`,
//! [`indexed`]) with `profile = "<path>"` in `clippy.toml` or in
//! `[workspace.metadata.blaze.lints]`, and the profile lints (`blaze::profile_hot`,
//! `blaze::stale_hot_marker`, `blaze::hot_marked_cold`) read from it which functions ran hot
//! and which never ran. With no profile named they are silent; a profile named that does not
//! read is an error naming its path, never lints run without it.
//!
//! A function's weight is the sum of its records' counters, of every instance of it and every
//! closure and shim inside it ([`names`]), as LLVM's `InstrProfSummaryBuilder` sums a record's
//! counts into the profile summary; its share is that weight over the whole profile's. It is
//! hot when its share is at least `profile-hot-percent` (1% by default) and cold when it is
//! below `profile-cold-percent` (0.0001% by default) or zero. A function the profile has no
//! name for is neither: the lints say nothing of it.
//!
//! Names are matched two ways. A profile of a program rustc built names its functions by their
//! symbols, demangled and folded onto the function written in source ([`names`]). A profile of
//! a program blaze built (blaze's own PGO profiles, `tools/pgo/profiles.json`) names each
//! instance `blaze.pgo.<hash>` of its item key and arguments ([`blaze_items::pgo`]), which is
//! not reversible: only an instance with no type or constant arguments is found, by hashing
//! the function's own name the same way.
//!
//! The profile's content ID is part of every key a lint result is cached under: the
//! configuration's ([`crate::conf::Conf`] hashes [`Loaded`] by it), so the query of a body
//! linted against one profile is never the answer for another.

pub(crate) mod indexed;
pub(crate) mod names;

use std::{alloc::Global, path::PathBuf, sync::Arc};

use base_db::Db;
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use hir::AsAssocItem as _;
use hir_def::{DefWithBodyId, FunctionId, ModuleDefId};

use crate::query;

/// The domain of a profile's [`Profile::id`].
pub const ID_DOMAIN: &str = "blaze v2 lint profile 1";

/// `profile-hot-percent`'s default: 1% of the profile's counts, in parts per billion.
pub const HOT_DEFAULT_PPB: u64 = 10_000_000;
/// `profile-cold-percent`'s default: 0.0001% of the profile's counts, in parts per billion.
pub const COLD_DEFAULT_PPB: u64 = 1_000;

/// The configuration keys of a profile, in `clippy.toml` and in
/// `[workspace.metadata.blaze.lints]`.
pub const KEYS: [&str; 3] = ["profile", "profile-hot-percent", "profile-cold-percent"];

/// Which profile the lints read and where a function is hot or cold in it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Settings {
    /// The profile, as an absolute path.
    pub path: PathBuf,
    /// Where it was named, for its errors: "`profile` in `<file>`".
    pub named_in: String,
    /// The share of the profile's counts at and above which a function is hot.
    pub hot_ppb: u64,
    /// The share below which a function is cold.
    pub cold_ppb: u64,
}

/// What a workspace's `[workspace.metadata.blaze.lints]` says of a profile: each key as
/// written (the percents as their TOML text), the path made absolute against the workspace's
/// root; `clippy.toml` overrides it key by key.
#[derive(Clone, Copy, Debug)]
pub struct Workspace<'a> {
    pub path: Option<&'a str>,
    pub hot_percent: Option<&'a str>,
    pub cold_percent: Option<&'a str>,
    /// The workspace's manifest, for errors.
    pub manifest: &'a str,
}

/// A percent of `key`, `text` its TOML number (`1`, `0.5`, `1e-4`), in parts per billion; an
/// error naming the key and `named_in` for anything but a number from 0 to 100.
pub fn percent_ppb(text: &str, key: &str, named_in: &str) -> Result<u64, String> {
    let percent: f64 = text.trim().parse().map_err(|_| {
        format!("`{key}` in {named_in} is `{text}`, not a percent (a number from 0 to 100)")
    })?;
    if !(0.0..=100.0).contains(&percent) {
        return Err(format!("`{key}` in {named_in} is {percent}, not a percent from 0 to 100"));
    }
    // A percent of 100 is 10^9 parts per billion: within `u64`, and never negative here.
    Ok((percent * 10_000_000.0).round() as u64)
}

/// A profile read, with the settings it is read under: what the configuration holds and is
/// keyed by, compared and hashed by the settings and the profile's content ID.
#[derive(Clone)]
pub struct Loaded {
    pub settings: Settings,
    pub profile: Arc<Profile>,
}

impl PartialEq for Loaded {
    fn eq(&self, other: &Loaded) -> bool {
        self.settings == other.settings && self.profile.id == other.profile.id
    }
}

impl Eq for Loaded {}

impl std::hash::Hash for Loaded {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.settings.hash(state);
        self.profile.id.hash(state);
    }
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded")
            .field("settings", &self.settings)
            .field("id", &format_args!("{:x}", self.profile.id))
            .finish()
    }
}

impl Loaded {
    /// The profile's file name, as the lints' messages name it.
    pub fn file_name(&self) -> String {
        let path = &self.settings.path;
        path.file_name()
            .map_or_else(|| path.display().to_string(), |it| it.to_string_lossy().into_owned())
    }

    /// How `count` of the profile's counts stands against its thresholds.
    pub fn hit(&self, agg: Agg) -> Hit {
        let total = self.profile.total;
        // `count / total` in parts per billion, rounded down; `total` is never zero ([`read`]).
        let share_ppb =
            u64::try_from(agg.count.saturating_mul(1_000_000_000) / total).unwrap_or(u64::MAX);
        Hit {
            count: agg.count,
            share_ppb,
            hot: share_ppb >= self.settings.hot_ppb,
            cold: agg.count == 0
                || agg.count.saturating_mul(1_000_000_000)
                    < u128::from(self.settings.cold_ppb) * total,
            instances: agg.instances,
        }
    }
}

/// A profile, as the lints read it.
#[derive(Debug)]
pub struct Profile {
    /// Of its bytes, in [`ID_DOMAIN`].
    pub id: ContentId,
    /// Its format and kind.
    pub header: Header,
    /// The sum of every record's counters.
    pub total: u128,
    /// How many records it has.
    pub records: u64,
    /// How many of its names neither demangle nor are blaze's: C functions and the like.
    pub foreign: u64,
    /// The weights of the names that demangle, by [`names`]' key.
    by_key: HashMap<Box<str>, Agg, Global>,
    /// The weights of blaze's own names, by their bytes.
    by_blaze: HashMap<[u8; 16], Agg, Global>,
}

pub use indexed::Header;

/// What the records of one function add to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Agg {
    /// The sum of their counters.
    pub count: u128,
    /// How many records: instances, closures and shims of the function.
    pub instances: u32,
}

impl Agg {
    fn add(&mut self, count: u128) {
        self.count = self.count.saturating_add(count);
        self.instances = self.instances.saturating_add(1);
    }
}

/// A function as the profile has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub count: u128,
    pub share_ppb: u64,
    pub hot: bool,
    pub cold: bool,
    pub instances: u32,
}

impl Hit {
    /// Its share as a percent, as the messages write it: `12.3%`, `0.004%`, `0%`.
    pub fn percent(self) -> String {
        let percent = self.share_ppb as f64 / 10_000_000.0;
        if self.share_ppb == 0 {
            "0%".to_owned()
        } else if percent >= 10.0 {
            format!("{percent:.0}%")
        } else if percent >= 0.1 {
            format!("{percent:.1}%")
        } else {
            format!("{percent:.4}%")
        }
    }
}

/// The profile of `settings`, read off the machine: an error naming the path and where it was
/// named when it can't be read, is no indexed profile, or has nothing counted.
pub fn load(settings: Settings) -> Result<Loaded, String> {
    let path = &settings.path;
    // Read off the disk by every job that lints with it, and counted as a look: no watch
    // vouches for it (`blaze_vfs::outside::looks`).
    blaze_vfs::outside::looked(path);
    let bytes = std::fs::read(path).map_err(|error| {
        format!(
            "the lint profile `{}` (`profile` in {}) can't be read: {error}",
            path.display(),
            settings.named_in
        )
    })?;
    let profile = read(&bytes).map_err(|what| {
        format!("the lint profile `{}` (`profile` in {}) {what}", path.display(), settings.named_in)
    })?;
    Ok(Loaded { settings, profile: Arc::new(profile) })
}

/// What one record's name is: blaze's own ([`blaze_items::pgo::parse_name`]), a symbol rustc
/// mangled ([`names::of_profile_name`]'s key), or neither (a C function, an edited name).
enum Named {
    Blaze([u8; 16]),
    Rustc(String),
    Foreign,
}

/// The hash of `name` when blaze named it ([`blaze_items::pgo::parse_name`]), after the
/// `file;` LLVM puts before the name of a function of internal linkage.
fn blaze_named(name: &[u8]) -> Option<[u8; 16]> {
    blaze_items::pgo::parse_name(name.rsplit(|it| *it == b';').next().unwrap_or(name))
}

fn named(name: &[u8]) -> Named {
    if let Some(name) = blaze_named(name) {
        Named::Blaze(name)
    } else if let Some(key) = names::of_profile_name(name) {
        Named::Rustc(key)
    } else {
        Named::Foreign
    }
}

/// How a profile's records are named ([`names`]): what tells a profile blaze made from one
/// rustc or clang made, or one edited by hand. A rustc symbol is one that demangles, so one
/// [`read`] finds no key for (and counts as foreign) is still rustc's here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Names {
    pub records: u64,
    /// Named `blaze.pgo.<hash>` ([`blaze_items::pgo::parse_name`]).
    pub blaze: u64,
    /// Symbols rustc mangled.
    pub rustc: u64,
    /// Neither: C and C++ functions clang named, or names edited.
    pub foreign: u64,
}

impl Names {
    /// Whether blaze made the profile: it has records, and blaze named every one.
    pub fn blaze_made(self) -> bool {
        self.records > 0 && self.blaze == self.records
    }
}

/// How the records of the profile `bytes` are named. Unlike [`read`], a profile whose counts
/// are all zero is no error: what names it holds is all that is asked, so a rustc symbol is
/// told by whether it demangles ([`names::is_mangled`]), never demangled into its key.
pub fn names(bytes: &[u8]) -> Result<Names, String> {
    let mut names = Names::default();
    indexed::read(bytes, |record| {
        names.records += 1;
        if blaze_named(record.name).is_some() {
            names.blaze += 1;
        } else if names::is_mangled(record.name) {
            names.rustc += 1;
        } else {
            names.foreign += 1;
        }
    })
    .map_err(|it| it.to_string())?;
    Ok(names)
}

/// The profile of `bytes`; the error says what is wrong with them.
pub fn read(bytes: &[u8]) -> Result<Profile, String> {
    let id = Hasher::new(ID_DOMAIN).update(bytes).finish();
    let mut by_key: HashMap<Box<str>, Agg, Global> = HashMap::new_in(Global);
    let mut by_blaze: HashMap<[u8; 16], Agg, Global> = HashMap::new_in(Global);
    let (mut total, mut records, mut foreign) = (0u128, 0u64, 0u64);
    let header = indexed::read(bytes, |record| {
        let count = record.counts().fold(0u128, |sum, it| sum.saturating_add(u128::from(it)));
        total = total.saturating_add(count);
        records += 1;
        match named(record.name) {
            Named::Blaze(name) => by_blaze.entry(name).or_default().add(count),
            // alloc: a key of the profile, once per distinct function it names
            Named::Rustc(key) => by_key.entry(key.into_boxed_str()).or_default().add(count),
            Named::Foreign => foreign += 1,
        }
    })
    .map_err(|it| it.to_string())?;
    if total == 0 {
        return Err(format!(
            "counts nothing in its {records} records: the profiled run executed none of the instrumented code, so no function is hotter than another"
        ));
    }
    Ok(Profile { id, header, total, records, foreign, by_key, by_blaze })
}

/// The profile's functions as one crate's functions are matched to them: [`query::profile_hits`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Hits {
    by_fn: HashMap<FunctionId, Hit, Global>,
    /// The crate's functions with a body.
    pub functions: u32,
    /// Those the profile has a name for.
    pub matched: u32,
    /// Those whose key another function of the crate writes too, matched to neither.
    pub ambiguous: u32,
    /// The profile's hot names in the crate's path that no function of it matched, hottest
    /// first (by key after), at most [`UNMATCHED_SHOWN`].
    pub unmatched_hot: Box<[(Box<str>, Hit)]>,
}

/// How many of [`Hits::unmatched_hot`] are kept.
pub const UNMATCHED_SHOWN: usize = 8;

impl Hits {
    pub fn of(&self, f: FunctionId) -> Option<Hit> {
        self.by_fn.get(&f).copied()
    }
}

/// The function `f` stands for, when it is one with a body of its own.
pub(crate) fn function_id(f: hir::Function) -> Option<FunctionId> {
    match ModuleDefId::try_from(hir::ModuleDef::Function(f)).ok()? {
        ModuleDefId::FunctionId(it) => Some(it),
        _ => None,
    }
}

/// Whether an instance of `f` has no type or constant arguments: neither it nor its impl has
/// such parameters, and it is no trait's method (generic over `Self`).
fn without_arguments(db: &Db, f: hir::Function) -> bool {
    let own = hir::GenericDef::Function(f).type_or_const_params(db).is_empty();
    own && match f.as_assoc_item(db).map(|it| it.container(db)) {
        None => true,
        Some(hir::AssocItemContainer::Impl(imp)) => {
            hir::GenericDef::Impl(imp).type_or_const_params(db).is_empty()
        }
        Some(hir::AssocItemContainer::Trait(_)) => false,
    }
}

/// [`query::profile_hits`]: every function with a body of `krate` ([`query::lint_plan`]),
/// matched by its blaze name and by its key.
pub(crate) fn hits_of(db: &Db, krate: base_db::Crate, loaded: &Loaded) -> Hits {
    let profile = &loaded.profile;
    let plan = query::lint_plan(db, krate);
    // alloc: the query's worklist, once per crate and profile
    let mut keyed: Vec<(FunctionId, Option<String>, Option<Agg>)> = Vec::new();
    let mut seen: HashMap<String, u32, Global> = HashMap::new_in(Global);
    for body in plan.bodies.iter() {
        let DefWithBodyId::FunctionId(id) = body.def else { continue };
        let f = hir::Function::from(id);
        let blaze = (!profile.by_blaze.is_empty() && without_arguments(db, f)).then(|| {
            let item = blaze_items::key::item_key(db, blaze_items::ItemId::Function(id));
            blaze_items::pgo::name_bytes(item, blaze_items::pgo::no_args())
        });
        let blaze = blaze.and_then(|it| profile.by_blaze.get(&it).copied());
        let key = names::of_function(db, f);
        if let Some(key) = &key {
            // clone: a function's key, once per function of the crate
            *seen.entry(key.clone()).or_default() += 1;
        }
        keyed.push((id, key, blaze));
    }
    let mut hits =
        Hits { functions: u32::try_from(keyed.len()).unwrap_or(u32::MAX), ..Hits::default() };
    let mut matched_keys: blaze_hash::HashSet<&str, Global> = blaze_hash::HashSet::new_in(Global);
    for (id, key, blaze) in &keyed {
        let unique = key.as_deref().filter(|it| seen.get(*it).copied() == Some(1));
        if key.is_some() && unique.is_none() {
            hits.ambiguous += 1;
        }
        let by_key = unique.and_then(|it| profile.by_key.get(it).copied());
        if let Some(it) = unique
            && by_key.is_some()
        {
            matched_keys.insert(it);
        }
        let agg = match (*blaze, by_key) {
            (None, None) => continue,
            (Some(a), None) | (None, Some(a)) => a,
            (Some(a), Some(b)) => Agg {
                count: a.count.saturating_add(b.count),
                instances: a.instances.saturating_add(b.instances),
            },
        };
        hits.matched += 1;
        hits.by_fn.insert(*id, loaded.hit(agg));
    }
    // The hot names that look like the crate's and matched nothing: what the coverage note
    // shows, so a mapping that misses is seen.
    if let Some(name) = hir::Crate::from(krate).display_name(db) {
        let name = name.crate_name().to_string();
        let (plain, qualified) = (format!("{name}::"), format!("<{name}::"));
        // alloc: the hot names unmatched, once per crate and profile
        let mut unmatched: Vec<(Box<str>, Hit)> = profile
            .by_key
            .iter()
            .filter(|(key, _)| {
                (key.starts_with(&plain) || key.starts_with(&qualified))
                    && !matched_keys.contains(&***key)
            })
            .map(|(key, agg)| (key.clone(), loaded.hit(*agg)))
            .filter(|(_, hit)| hit.hot)
            .collect();
        unmatched.sort_unstable_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(&b.0)));
        unmatched.truncate(UNMATCHED_SHOWN);
        hits.unmatched_hot = unmatched.into_boxed_slice();
    }
    hits
}

/// How well a profile covers `krate`: its [`Hits`] under the profile `conf` names, if it
/// names one.
pub fn coverage<'db>(
    db: &'db Db,
    krate: hir::Crate,
    conf: &crate::conf::Conf,
) -> Option<&'db Hits> {
    let loaded = conf.profile.as_ref()?;
    // clone: the profile's settings and a refcount, once per crate
    Some(query::profile_hits(db, krate.base(), query::ProfileKey::new(db, loaded.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percents_read_as_numbers_from_0_to_100() {
        assert_eq!(percent_ppb("1", "profile-hot-percent", "x").unwrap(), HOT_DEFAULT_PPB);
        assert_eq!(percent_ppb("0.0001", "profile-cold-percent", "x").unwrap(), COLD_DEFAULT_PPB);
        assert!(
            percent_ppb("101", "profile-hot-percent", "x").unwrap_err().contains("from 0 to 100")
        );
        assert!(
            percent_ppb("\"hot\"", "profile-hot-percent", "x")
                .unwrap_err()
                .contains("not a percent")
        );
    }

    #[test]
    fn a_profile_folds_instances_and_counts_its_total() {
        let legacy = "_ZN3app3run17h0123456789abcdefE";
        let bytes =
            indexed::written(indexed::IR_PROF, &[(legacy, 1, &[90, 10]), ("main", 2, &[100])]);
        let profile = read(&bytes).unwrap();
        assert_eq!(profile.total, 200);
        assert_eq!(profile.foreign, 1);
        assert_eq!(profile.by_key.get("app::run").copied(), Some(Agg { count: 100, instances: 1 }));
        let empty = indexed::written(indexed::IR_PROF, &[(legacy, 1, &[0])]);
        assert!(read(&empty).unwrap_err().contains("counts nothing"));
    }

    /// [`names`], which tells a profile blaze made from one rustc made: each name sorted by
    /// who named it, after the `file;` of a function of internal linkage, and a profile of
    /// zero counts read too.
    #[test]
    fn a_profiles_names_are_sorted_by_who_made_them() {
        let blaze =
            format!("{}{}", blaze_items::pgo::NAME_PREFIX, "0123456789abcdef0123456789abcdef");
        let rustc = "_ZN3app3run17h0123456789abcdefE";
        let internal = "src/lib.rs;_ZN3app4stop17h0123456789abcdefE";
        let records: &[(&str, u64, &[u64])] =
            &[(&blaze, 1, &[0]), (rustc, 2, &[0]), (internal, 3, &[0]), ("main", 4, &[0])];
        let bytes = indexed::written(indexed::IR_PROF, records);
        let names = names(&bytes).unwrap();
        assert_eq!(names, Names { records: 4, blaze: 1, rustc: 2, foreign: 1 });
        assert!(!names.blaze_made());
        let only = indexed::written(indexed::IR_PROF, &[(&blaze, 1, &[0])]);
        assert!(super::names(&only).unwrap().blaze_made());
    }
}
