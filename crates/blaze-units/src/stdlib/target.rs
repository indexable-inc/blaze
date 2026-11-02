//! What the pinned nightly's rustc says of each target blaze builds for, without running it
//! (docs/no-rustc.md, step F): the cfgs `rustc --print cfg` prints, with those its arguments
//! decide, the default panic strategy, whether the target has std, and the names of its files.
//! The data is what that rustc printed (`rustc.json`, written by `harness/answers.py --write`
//! when the pin moves); the cfgs that follow from the arguments are added here as rustc adds
//! them (`rustc_session::config::cfg::default_configuration`, `rustc_codegen_ssa`'s
//! `target_features`). A triple the data does not have is an error naming it, never a guess.
//!
//! The JSON is read once per process and compiled per target into columns: feature names
//! interned to bits of a [`Features`] in their sorted order, what each turns on and off as a set,
//! the target spec's facts and every `target_feature="..."` line made once. [`target_cfg`] is
//! then a binary search for the triple, a few word operations per `-C target-feature`, and the
//! copy of the output; it reads no lock (a `OnceLock` once set is one atomic load).

use std::{borrow::Cow, collections::BTreeMap, sync::OnceLock};

use blaze_ids::bits::BitArray;
use blaze_resolve::Cfg;
use serde::Deserialize;

const DATA: &str = include_str!("rustc.json");

#[derive(Deserialize)]
struct RawRelease {
    /// As the first line of `rustc -V` has it, after `rustc `.
    version: String,
    /// What `rustc -vV` prints, with `{host}` for the host's triple.
    verbose: String,
    /// The warning of `-C target-feature=+{feature}` of a feature neither rustc nor LLVM has
    /// of the target, with `{feature}` for its name.
    #[serde(default)]
    unknown_feature: Option<String>,
    targets: BTreeMap<String, RawTarget>,
}

#[derive(Deserialize)]
struct RawTarget {
    /// The cfgs of the target, but those the arguments decide.
    cfg: Vec<String>,
    /// The cfgs printed besides those when unstable features are allowed, as on blaze's
    /// nightly they always are.
    nightly_cfg: Vec<String>,
    /// The strategy of `-C panic` when none is given.
    panic: String,
    split_debuginfo: Vec<String>,
    /// The name of the file of each kind of crate named `___`; `None` for a kind the target
    /// does not have.
    file_names: BTreeMap<String, Option<String>>,
    /// By the host rustc ran on.
    target_spec_json: BTreeMap<String, String>,
    /// The `target_feature`s each feature rustc has turns on beside the target's own, with
    /// itself.
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
    /// The same of the features only LLVM names, which `target-cpu=native` turns on.
    #[serde(default)]
    llvm_features: BTreeMap<String, Vec<String>>,
    /// Of each feature the target has, the features turning it off turns off: itself and
    /// those that need it.
    #[serde(default)]
    features_off: BTreeMap<String, Vec<String>>,
    /// Each CPU of `-C target-cpu`, by the index of its `target_feature`s in `cpu_features`.
    #[serde(default)]
    cpus: BTreeMap<String, usize>,
    #[serde(default)]
    cpu_features: Vec<Vec<String>>,
    /// The cfgs besides `target_feature` a feature has: wasm's `atomics`, `target_has_threads`.
    #[serde(default)]
    feature_cfgs: BTreeMap<String, Vec<String>>,
    /// The other features rustc or LLVM lists of the target (`--print target-features`): one
    /// of these turns no cfg on or off, and rustc warns of it.
    #[serde(default)]
    other_features: Vec<String>,
}

#[derive(Deserialize)]
struct RawSpec {
    #[serde(default, rename = "crt-static-respected")]
    crt_static_respected: bool,
    #[serde(rename = "data-layout")]
    data_layout: Option<String>,
    /// rustc's spec JSON leaves out an option at its default, here false.
    #[serde(default, rename = "has-thumb-interworking")]
    has_thumb_interworking: bool,
    #[serde(default)]
    metadata: RawSpecMetadata,
}

#[derive(Deserialize, Default)]
struct RawSpecMetadata {
    #[serde(default)]
    std: Option<bool>,
}

/// The most feature names a target may have that can be a `target_feature` cfg (the largest
/// recorded, aarch64's, has 92).
const MAX_FEATURES: usize = 256;

/// A set of a target's feature names, by their index in [`Compiled::names`]: 32 bytes, so a
/// union, difference or subset test is four word operations.
pub(super) type Features = BitArray<{ MAX_FEATURES / 64 }, u16>;

const _: () = assert!(std::mem::size_of::<Features>() == 32);

/// What a target's spec says that a build needs, read once.
struct SpecFacts {
    crt_static_respected: bool,
    std: Option<bool>,
    /// LLVM's data layout of the target, as the spec has it.
    data_layout: Option<Box<str>>,
    has_thumb_interworking: bool,
}

/// One target's data, compiled. Lookups are binary searches of sorted columns (a few dozen
/// entries at most); sets are [`Features`] over `names`.
struct Compiled {
    /// The recorded cfgs but `target_feature`s (`cfg`, then `nightly_cfg`).
    base: Box<[Box<str>]>,
    /// How many of `base` are of `cfg`: what rustc prints where unstable features are not
    /// allowed.
    stable_base: usize,
    /// Every name a feature set can hold, sorted: a name's index is its bit.
    names: Box<[Box<str>]>,
    /// `target_feature="<name>"` of each name, in `names`' order.
    feature_lines: Box<[Box<str>]>,
    /// The target's own `target_feature`s.
    own: Features,
    /// Those of them `cfg` has, printed where unstable features are not allowed.
    own_stable: Features,
    /// `crt-static`'s index, when a set can hold it.
    crt_static: Option<u16>,
    /// What turning a name on turns on (`features`, else `llvm_features`), per name; empty
    /// for a name of neither, which `has_on` tells apart.
    on: Box<[Features]>,
    has_on: Features,
    /// The names `features` (rustc's own list) has.
    rustc_features: Features,
    /// What turning a name off turns off (`features_off`), per name.
    off: Box<[Features]>,
    has_off: Features,
    /// Per name, the target's own features that need it (those whose `features_off` holds it),
    /// turned back on with it.
    pulls: Box<[Features]>,
    /// Each CPU's features, by name, sorted; `None` for an index past `cpu_features`.
    cpus: Box<[(Box<str>, Option<Features>)]>,
    /// The other cfgs a name brings, by its index, ascending.
    feature_cfgs: Box<[(u16, Box<[Box<str>]>)]>,
    /// The other features rustc or LLVM lists of the target (`other_features`), sorted. Empty
    /// where the data does not list them: then a feature it does not name can't be told from
    /// one blaze has no data of.
    other_features: Box<[Box<str>]>,
    panic: Box<str>,
    split_debuginfo: Box<[Box<str>]>,
    /// By crate type, sorted.
    file_names: Box<[(Box<str>, Option<Box<str>>)]>,
    /// `rustc --print target-spec-json` by the host it ran on, sorted.
    spec_json: Box<[(Box<str>, Box<str>)]>,
    spec: Result<SpecFacts, String>,
}

struct Release {
    version: String,
    verbose: String,
    unknown_feature: Option<String>,
    /// By triple, sorted.
    targets: Box<[(Box<str>, Result<Compiled, String>)]>,
}

fn boxed(items: impl IntoIterator<Item = String>) -> Box<[Box<str>]> {
    items.into_iter().map(String::into_boxed_str).collect()
}

const FEATURE_PREFIX: &str = "target_feature=\"";

fn feature_of(cfg: &str) -> Option<&str> {
    cfg.strip_prefix(FEATURE_PREFIX)?.strip_suffix('"')
}

fn compile(triple: &str, raw: RawTarget) -> Result<Compiled, String> {
    let RawTarget {
        cfg,
        nightly_cfg,
        panic,
        split_debuginfo,
        file_names,
        target_spec_json,
        features,
        llvm_features,
        features_off,
        cpus,
        cpu_features,
        feature_cfgs,
        other_features,
    } = raw;
    let stable_lines = cfg.len();
    let recorded: Vec<String> = cfg.into_iter().chain(nightly_cfg).collect();
    let mut names: Vec<&str> = recorded.iter().filter_map(|it| feature_of(it)).collect();
    for map in [&features, &llvm_features, &features_off] {
        for (name, turned) in map {
            names.push(name);
            names.extend(turned.iter().map(String::as_str));
        }
    }
    names.extend(cpu_features.iter().flatten().map(String::as_str));
    names.extend(feature_cfgs.keys().map(String::as_str));
    names.sort_unstable();
    names.dedup();
    if names.len() > MAX_FEATURES {
        return Err(format!(
            "blaze's data of target `{triple}` has {} features that can be cfgs, past the {MAX_FEATURES} \
             a feature set holds",
            names.len()
        ));
    }
    let index = |name: &str| -> u16 {
        names.binary_search(&name).expect("every name of a set was interned above") as u16
    };
    let set = |list: &[String]| {
        let mut bits = Features::default();
        for name in list {
            bits.insert(index(name));
        }
        bits
    };
    let count = names.len();
    let (mut own, mut own_stable) = (Features::default(), Features::default());
    let mut base = Vec::with_capacity(recorded.len());
    let mut stable_base = 0;
    // `cfg`'s lines come first, so `base[..stable_base]` are its.
    for (at, line) in recorded.iter().enumerate() {
        let stable = at < stable_lines;
        match feature_of(line) {
            Some(name) => {
                own.insert(index(name));
                if stable {
                    own_stable.insert(index(name));
                }
            }
            None => {
                base.push(line.clone().into_boxed_str());
                stable_base += usize::from(stable);
            }
        }
    }
    let (mut on, mut has_on, mut rustc_features) =
        (vec![Features::default(); count], Features::default(), Features::default());
    // rustc's list wins over LLVM's for a name both have.
    for (name, turned) in &llvm_features {
        let at = index(name);
        on[usize::from(at)] = set(turned.as_slice());
        has_on.insert(at);
    }
    for (name, turned) in &features {
        let at = index(name);
        on[usize::from(at)] = set(turned.as_slice());
        has_on.insert(at);
        rustc_features.insert(at);
    }
    let (mut off, mut has_off, mut pulls) =
        (vec![Features::default(); count], Features::default(), vec![Features::default(); count]);
    for (name, turned) in &features_off {
        let at = index(name);
        off[usize::from(at)] = set(turned.as_slice());
        has_off.insert(at);
        for needed in turned {
            pulls[usize::from(index(needed))].insert(at);
        }
    }
    let cpus = cpus
        .iter()
        .map(|(name, at)| (name.clone().into_boxed_str(), cpu_features.get(*at).map(|it| set(it.as_slice()))))
        .collect();
    let feature_cfgs = feature_cfgs.iter().map(|(name, cfgs)| (index(name), boxed(cfgs.iter().cloned()))).collect();
    let spec = match target_spec_json.values().next() {
        None => Err(format!("blaze recorded no target spec of `{triple}`")),
        Some(text) => serde_json::from_str::<RawSpec>(text)
            .map(|it| SpecFacts {
                crt_static_respected: it.crt_static_respected,
                std: it.metadata.std,
                data_layout: it.data_layout.map(String::into_boxed_str),
                has_thumb_interworking: it.has_thumb_interworking,
            })
            .map_err(|error| format!("the recorded target spec of `{triple}`: {error}")),
    };
    let crt_static = names.binary_search(&"crt-static").ok().map(|it| it as u16);
    let feature_lines = names.iter().map(|it| format!("{FEATURE_PREFIX}{it}\"").into_boxed_str()).collect();
    let names = names.iter().map(|it| Box::<str>::from(*it)).collect();
    // alloc: a target's columns, compiled once per process
    let mut other_features: Vec<Box<str>> = other_features.into_iter().map(String::into_boxed_str).collect();
    other_features.sort_unstable();
    Ok(Compiled {
        base: base.into_boxed_slice(),
        stable_base,
        names,
        feature_lines,
        own,
        own_stable,
        crt_static,
        on: on.into_boxed_slice(),
        has_on,
        rustc_features,
        off: off.into_boxed_slice(),
        has_off,
        pulls: pulls.into_boxed_slice(),
        cpus,
        feature_cfgs,
        other_features: other_features.into_boxed_slice(),
        panic: panic.into_boxed_str(),
        split_debuginfo: boxed(split_debuginfo),
        file_names: file_names.into_iter().map(|(kind, name)| (kind.into_boxed_str(), name.map(String::into_boxed_str))).collect(),
        spec_json: target_spec_json
            .into_iter()
            .map(|(host, spec)| (host.into_boxed_str(), spec.into_boxed_str()))
            .collect(), // alloc: a target's columns, compiled once per process
        spec,
    })
}

fn release() -> &'static Release {
    static READ: OnceLock<Release> = OnceLock::new();
    READ.get_or_init(|| {
        let raw: RawRelease =
            serde_json::from_str(DATA).expect("stdlib/rustc.json is what harness/answers.py writes");
        Release {
            version: raw.version,
            verbose: raw.verbose,
            unknown_feature: raw.unknown_feature,
            targets: raw
                .targets
                .into_iter()
                .map(|(triple, target)| {
                    let compiled = compile(&triple, target);
                    (triple.into_boxed_str(), compiled)
                })
                .collect(),
        }
    })
}

fn target(triple: &str) -> Result<&'static Compiled, String> {
    let targets = &release().targets;
    match targets.binary_search_by(|(it, _)| (**it).cmp(triple)) {
        Ok(at) => targets[at].1.as_ref().map_err(Clone::clone),
        Err(_) => {
            let known: Vec<&str> = targets.iter().map(|(it, _)| &**it).collect();
            Err(format!(
                "blaze has no data of target `{triple}` (it knows {}): harness/answers.py records each \
                 target blaze builds for, and blaze runs no rustc to ask (docs/no-rustc.md)",
                known.join(", ")
            ))
        }
    }
}

impl Compiled {
    fn index(&self, name: &str) -> Option<u16> {
        self.names.binary_search_by(|it| (**it).cmp(name)).ok().map(|it| it as u16)
    }

    fn spec(&self) -> Result<&SpecFacts, String> {
        // alloc: the error's text, only for a target whose recorded spec did not parse.
        self.spec.as_ref().map_err(Clone::clone)
    }

    /// One `-C target-feature` (or, `from_host`, one of the host's for `target-cpu=native`)
    /// applied to `have`, as rustc's `cfg_target_feature` does.
    fn apply(&self, triple: &str, have: &mut Features, enable: bool, name: &str, from_host: bool) -> Result<(), String> {
        let at = self.index(name);
        let on = at.filter(|it| self.has_on.contains(*it));
        match (enable, on, at) {
            (true, Some(it), _) => {
                have.union_with(&self.on[usize::from(it)]);
                // The target's own features it needs, should they be off.
                have.union_with(&self.pulls[usize::from(it)]);
            }
            (false, _, Some(it)) if self.has_off.contains(it) => {
                have.subtract(&self.off[usize::from(it)]);
            }
            (false, Some(it), _) if !self.own.contains(it) => {
                // Off with it go the features that turn it on.
                let turned = &self.on[usize::from(it)];
                if !turned.is_empty() {
                    // A copy, so `have` loses members while it is walked.
                    for other in (*have).and(&self.rustc_features).iter() {
                        if turned.is_subset(&self.on[usize::from(other)]) {
                            have.remove(other);
                        }
                    }
                }
            }
            // A feature only LLVM has, of no `target_feature`, changes none of them.
            _ if from_host => {}
            _ => {
                let sign = if enable { '+' } else { '-' };
                return Err(format!(
                    "blaze's data of target `{triple}` does not say what `-C target-feature={sign}{name}` \
                     turns {}",
                    if enable { "on" } else { "off" }
                ));
            }
        }
        Ok(())
    }

    /// The `target_feature`s with `cpu`'s and then `features` turned on (`true`) and off in
    /// order, starting from the target's own; the features of `features` that change none go
    /// in `skipped`, in order.
    fn tuned<'a>(
        &self,
        triple: &str,
        cpu: Option<(&str, Option<&str>)>,
        features: &'a [(bool, String)],
        skipped: &mut Skipped<'a>,
    ) -> Result<Features, String> {
        let mut have = match cpu {
            Some((name, _)) => {
                let at = self
                    .cpus
                    .binary_search_by(|(it, _)| (**it).cmp(name))
                    .map_err(|_| format!("`-C target-cpu={name}` is no CPU blaze knows of target `{triple}`"))?;
                self.cpus[at]
                    .1
                    .ok_or_else(|| format!("blaze's data of target `{triple}` lists no features of CPU `{name}`"))?
            }
            None => self.own,
        };
        // `target-cpu=native`: LLVM's features of the host, which name what rustc does not too.
        if let Some(host) = cpu.and_then(|(_, host)| host) {
            for it in host.split(',') {
                if let Some(name) = it.get(1..) {
                    self.apply(triple, &mut have, it.starts_with('+'), name, true)?;
                }
            }
        }
        for (enable, name) in features {
            let listed = self.index(name).is_some_and(|it| self.has_on.contains(it) || self.has_off.contains(it));
            if !listed {
                if self.other_features.is_empty() {
                    return Err(format!(
                        "blaze's data of target `{triple}` does not list its features, so not whether \
                         `{name}` is one"
                    ));
                }
                // rustc warns of a feature it does not know, or knows of no cfg, and passes it to
                // LLVM: no cfg.
                match self.other_features.binary_search_by(|it| (**it).cmp(name.as_str())) {
                    Ok(_) => skipped.listed.push(name.as_str()),
                    Err(_) => skipped.unknown.push(name.as_str()),
                }
                continue;
            }
            self.apply(triple, &mut have, *enable, name, false)?;
        }
        Ok(have)
    }
}

/// The host's CPU as LLVM names it, for `-C target-cpu=native`: what the daemon reads of the
/// machine it runs on (`hir_ty_codegen::host_cpu`), never a query.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostCpu {
    /// The architecture of the host's triple (what is before its first `-`).
    pub arch: String,
    pub name: String,
    /// LLVM's features of it, `+a,-b,...`.
    pub features: String,
}

/// The `-C target-feature`s that turn no cfg on or off, which rustc warns of and passes on to
/// LLVM, in order, as given.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Skipped<'a> {
    /// Those neither rustc nor LLVM lists of the target, whose warning is
    /// [`unknown_feature_warning`]'s.
    pub unknown: Vec<&'a str>,
    /// Those rustc or LLVM lists of the target (`other_features`), whose warnings the data
    /// does not have.
    pub listed: Vec<&'a str>,
}

/// The arguments of a rustc that decide cfgs: the flags of `-C`, `--cfg` and the crate types,
/// as [`CfgArgs::from_flags`] reads them from a command line (cargo's rustflags).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct CfgArgs {
    pub opt_level: Option<String>,
    pub debug_assertions: Option<bool>,
    pub overflow_checks: Option<bool>,
    pub panic: Option<String>,
    pub target_cpu: Option<String>,
    /// For `target_cpu` `native`.
    pub host_cpu: Option<HostCpu>,
    /// `-C target-feature`'s, on (`true`) or off, in order.
    pub target_features: Vec<(bool, String)>,
    /// `--cfg`'s, as given (`foo`, `feature="x"`).
    pub cfgs: Vec<String>,
    /// Whether `proc-macro` is among the crate types: cargo's probe of a target
    /// (`TargetInfo::new`) has it, so cargo's cfgs of a target have `proc_macro`.
    pub proc_macro: bool,
    /// The sanitizer `-Z sanitizer` turns on (its lists add up; [`SANITIZERS`] exclude each
    /// other), which rustc sets `cfg(sanitize = "..")` for.
    pub sanitizer: Option<&'static str>,
    /// Whether rustc is asked as on a channel that allows no unstable features
    /// (`RUSTC_BOOTSTRAP=-1`): its cfgs of unstable features (`nightly_cfg`, `overflow_checks`,
    /// `ub_checks`, `sanitize`) are left out, and tuning (`target_cpu`, `target_features`) is
    /// refused, the data having the features of the nightly only.
    pub stable: bool,
}

/// The sanitizers blaze's code generator instruments with, sorted; any two exclude each other,
/// as in rustc. `blaze_cargo::unstable` reads `-Z sanitizer` by these too.
pub const SANITIZERS: [&str; 3] = ["address", "memory", "thread"];

/// Sanitizers rustc has that blaze's code generator does not instrument with.
pub const UNSANITIZED: [&str; 11] = [
    "cfi",
    "dataflow",
    "hwaddress",
    "kcfi",
    "kernel-address",
    "kernel-hwaddress",
    "leak",
    "memtag",
    "realtime",
    "safestack",
    "shadow-call-stack",
];

/// The codegen options that decide no cfg.
const NO_CFG_CODEGEN: [&str; 22] = [
    "passes",
    "llvm-args",
    "embed-bitcode",
    "debuginfo",
    "metadata",
    "extra-filename",
    "incremental",
    "strip",
    "codegen-units",
    "prefer-dynamic",
    "lto",
    "linker",
    "linker-features",
    "link-arg",
    "link-args",
    "split-debuginfo",
    "link-dead-code",
    "rpath",
    "force-frame-pointers",
    "force-unwind-tables",
    "symbol-mangling-version",
    "save-temps",
];

/// The flags that take a value and decide no cfg.
const NO_CFG_VALUED: [&str; 22] = [
    "--edition",
    "--error-format",
    "--json",
    "--emit",
    "--out-dir",
    "-o",
    "-L",
    "-l",
    "--extern",
    "--check-cfg",
    "--cap-lints",
    "--color",
    "--diagnostic-width",
    "--remap-path-prefix",
    "--crate-name",
    "-W",
    "-A",
    "-D",
    "-F",
    "--warn",
    "--allow",
    "--deny",
];

const NO_CFG_LINTS: [&str; 2] = ["--forbid", "--force-warn"];

fn yes(flag: &str, value: Option<&str>) -> Result<bool, String> {
    match value {
        None | Some("y" | "yes" | "on" | "true") => Ok(true),
        Some("n" | "no" | "off" | "false") => Ok(false),
        Some(other) => Err(format!("`-C {flag}={other}`: rustc takes a boolean (`y`, `yes`, `on`, `true`, `n`, `no`, `off`, `false`)")),
    }
}

impl CfgArgs {
    /// The arguments of `flags`, a rustc command line (cargo's rustflags for a target). A flag
    /// that may decide a cfg and is not read here is an error naming it.
    pub fn from_flags(flags: &[String]) -> Result<CfgArgs, String> {
        let mut args = CfgArgs::default();
        let mut words = flags.iter().map(String::as_str);
        while let Some(word) = words.next() {
            let (flag, joined) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
                _ => match word.as_bytes() {
                    [b'-', b'C' | b'Z' | b'W' | b'A' | b'D' | b'F' | b'L' | b'l', _, ..] => (&word[..2], Some(&word[2..])),
                    _ => (word, None),
                },
            };
            let mut value = || {
                joined
                    .or_else(|| words.next())
                    .ok_or_else(|| format!("rustc's `{flag}` takes a value, and the flags end after it"))
            };
            match flag {
                "--cfg" => args.cfgs.push(value()?.to_owned()),
                "--crate-type" => args.proc_macro |= value()?.split(',').any(|it| it == "proc-macro"),
                "-O" => args.opt_level = Some("2".to_owned()),
                "-g" => {}
                "-C" | "--codegen" => {
                    let option = value()?;
                    let (name, value) = match option.split_once('=') {
                        Some((name, value)) => (name, Some(value)),
                        None => (option, None),
                    };
                    let name = if name.contains('_') { Cow::Owned(name.replace('_', "-")) } else { Cow::Borrowed(name) };
                    let needed = |value: Option<&str>| {
                        value.map(str::to_owned).ok_or_else(|| format!("rustc's `-C {name}` takes a value"))
                    };
                    match &*name {
                        "opt-level" => args.opt_level = Some(needed(value)?),
                        "debug-assertions" => args.debug_assertions = Some(yes(&name, value)?),
                        "overflow-checks" => args.overflow_checks = Some(yes(&name, value)?),
                        "panic" => args.panic = Some(needed(value)?),
                        "target-cpu" => args.target_cpu = Some(needed(value)?),
                        "target-feature" => {
                            let list = value.ok_or_else(|| format!("rustc's `-C {name}` takes a value"))?;
                            for feature in list.split(',').filter(|it| !it.is_empty()) {
                                let enable = match feature.as_bytes()[0] {
                                    b'+' => true,
                                    b'-' => false,
                                    _ => {
                                        return Err(format!(
                                            "`-C target-feature={feature}`: each feature starts with `+` or `-`"
                                        ));
                                    }
                                };
                                args.target_features.push((enable, feature[1..].to_owned()));
                            }
                        }
                        other if NO_CFG_CODEGEN.contains(&other) => {}
                        other => return Err(format!("blaze does not know which cfgs `-C {other}` decides")),
                    }
                }
                "-Z" => {
                    let option = value()?;
                    let (name, value) = match option.split_once('=') {
                        Some((name, value)) => (name, Some(value)),
                        None => (option, None),
                    };
                    let name = if name.contains('_') { Cow::Owned(name.replace('_', "-")) } else { Cow::Borrowed(name) };
                    match &*name {
                        "unstable-options" | "threads" | "macro-backtrace" | "time-passes" | "share-generics"
                        | "external-clangrt" | "linker-features" | "sanitizer-recover"
                        | "sanitizer-memory-track-origins" => {}
                        "sanitizer" => {
                            let list = value.ok_or_else(|| {
                                format!("-Z {option}: rustc wants a comma-separated list of sanitizers")
                            })?;
                            for name in list.split(',') {
                                let on = SANITIZERS[sanitizer(name, option)?];
                                match args.sanitizer {
                                    Some(was) if was != on => {
                                        let (a, b) = if was < on { (was, on) } else { (on, was) };
                                        return Err(format!(
                                            "`-Zsanitizer={a}` is incompatible with `-Zsanitizer={b}`"
                                        ));
                                    }
                                    _ => args.sanitizer = Some(on),
                                }
                            }
                        }
                        other => return Err(format!("blaze does not know which cfgs `-Z {other}` decides")),
                    }
                }
                _ if NO_CFG_VALUED.contains(&flag) || NO_CFG_LINTS.contains(&flag) => {
                    value()?;
                }
                _ if !flag.starts_with('-') => {}
                other => return Err(format!("blaze does not know which cfgs rustc's `{other}` decides")),
            }
        }
        Ok(args)
    }

    fn debug_assertions(&self) -> bool {
        self.debug_assertions.unwrap_or(matches!(self.opt_level.as_deref(), None | Some("0")))
    }
}

/// A name of `-Z sanitizer`'s list (`given`) as rustc reads it (`parse_sanitizers`): the index
/// in [`SANITIZERS`] of one blaze instruments with, else an error naming it.
pub fn sanitizer(name: &str, given: &str) -> Result<usize, String> {
    match SANITIZERS.iter().position(|it| *it == name) {
        Some(at) => Ok(at),
        None if UNSANITIZED.contains(&name) => {
            Err(format!("-Z {given}: blaze's code generator does not instrument with the `{name}` sanitizer"))
        }
        None => Err(format!("-Z {given}: rustc has no sanitizer `{name}`")),
    }
}

/// The cfgs rustc sets itself, which it refuses as `--cfg`, by name or by what it starts with.
const BUILTIN_CFGS: [&str; 13] = [
    "debug_assertions",
    "overflow_checks",
    "ub_checks",
    "contract_checks",
    "fmt_debug",
    "relocation_model",
    "panic",
    "unix",
    "windows",
    "test",
    "emscripten_wasm_eh",
    "sanitize",
    "sanitizer_",
];

/// Whether `cfg` is `name` or `name="value"` as rustc prints it back.
fn is_plain_cfg(cfg: &str) -> bool {
    let (name, value) = match cfg.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (cfg, None),
    };
    let plain_name = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let builtin = name.starts_with("target_")
        || BUILTIN_CFGS.iter().any(|it| name == *it || (it.ends_with('_') && name.starts_with(it)));
    let plain_value = value.is_none_or(|value| {
        let inner = value.strip_prefix('"').and_then(|it| it.strip_suffix('"'));
        inner.is_some_and(|it| it.chars().all(|c| c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' '))
    });
    plain_name && !builtin && plain_value
}

/// The architecture of a triple: what is before its first `-`.
pub fn arch(triple: &str) -> &str {
    triple.split('-').next().unwrap_or(triple)
}

/// The cfgs `rustc --print cfg --target <triple> <args>` prints, one per line as rustc prints
/// them, sorted. Call it once per platform and profile: it copies its answer out.
pub fn target_cfg(triple: &str, args: &CfgArgs) -> Result<Vec<String>, String> {
    target_cfg_told(triple, args).map(|(cfgs, _)| cfgs)
}

/// [`target_cfg`], and the features of `args` that change no cfg, which rustc warns of.
pub fn target_cfg_told<'a>(triple: &str, args: &'a CfgArgs) -> Result<(Vec<String>, Skipped<'a>), String> {
    let target = target(triple)?;
    let (base, own) = match args.stable {
        true => (&target.base[..target.stable_base], target.own_stable),
        false => (&target.base[..], target.own),
    };
    let static_default = target.crt_static.is_some_and(|it| own.contains(it));
    let as_it_is = args
        .target_features
        .iter()
        .all(|(enable, name)| name == "crt-static" && *enable == static_default);
    let tuning = args.target_cpu.is_some() || !as_it_is;
    let mut skipped = Skipped::default();
    let mut features = if tuning {
        if args.stable {
            // alloc: the error's text
            return Err(format!(
                "`-C target-cpu` or `-C target-feature` for target `{triple}` where unstable features \
                 are not allowed: blaze's data has what they turn on for the nightly only"
            ));
        }
        let cpu = match args.target_cpu.as_deref() {
            Some("native") => match &args.host_cpu {
                Some(host) if host.arch == arch(triple) => Some((host.name.as_str(), Some(host.features.as_str()))),
                Some(host) => {
                    return Err(format!(
                        "`-C target-cpu=native` for target `{triple}` on a {} host: rustc's native CPU \
                         is the host's, of another architecture",
                        host.arch
                    ));
                }
                None => {
                    return Err(format!(
                        "`-C target-cpu=native` for target `{triple}`: the host's CPU was not given"
                    ));
                }
            },
            Some(name) => Some((name, None)),
            None => None,
        };
        target.tuned(triple, cpu, &args.target_features, &mut skipped)?
    } else {
        own
    };
    let mut cfgs = Vec::with_capacity(base.len() + features.count() + args.cfgs.len() + 6);
    cfgs.extend(base.iter().map(|it| String::from(&**it)));
    if tuning {
        for (at, extra) in target.feature_cfgs.iter() {
            if features.contains(*at) {
                cfgs.extend(extra.iter().map(|it| String::from(&**it)));
            }
        }
    }
    // `ub_checks`, `overflow_checks` and `sanitize` are cfgs of unstable features, which
    // rustc prints only where those are allowed.
    let debug_assertions = args.debug_assertions();
    if debug_assertions {
        cfgs.push("debug_assertions".to_owned());
        if !args.stable {
            cfgs.push("ub_checks".to_owned());
        }
    }
    if !args.stable && args.overflow_checks.unwrap_or(debug_assertions) {
        cfgs.push("overflow_checks".to_owned());
    }
    let panic = args.panic.as_deref().unwrap_or(&target.panic);
    if panic != &*target.panic && panic != "abort" {
        return Err(format!(
            "`-C panic={panic}` for target `{triple}`, whose strategy is `{}`: blaze knows only `abort` \
             besides a target's own",
            target.panic
        ));
    }
    cfgs.push(format!("panic=\"{panic}\""));
    // rustc's `cfg(sanitize)` of each sanitizer on; `cfg(sanitizer_cfi_*)` follow only from
    // the `cfi` sanitizer, which `sanitizer` refuses.
    if let Some(name) = args.sanitizer {
        if args.stable {
            // alloc: the error's text
            return Err(format!("`-Z sanitizer={name}` where unstable features are not allowed"));
        }
        cfgs.push(format!("sanitize=\"{name}\""));
    }
    if args.proc_macro {
        cfgs.push("proc_macro".to_owned());
        // A proc macro is a dynamic library, which a target that respects `crt-static` builds
        // without it unless it is asked for (rustc's `Session::crt_static`).
        if !args.target_features.iter().any(|(_, name)| name == "crt-static") && target.spec()?.crt_static_respected {
            if let Some(it) = target.crt_static {
                features.remove(it);
            }
        }
    }
    cfgs.extend(features.iter().map(|it| String::from(&*target.feature_lines[usize::from(it)])));
    for cfg in &args.cfgs {
        if !is_plain_cfg(cfg) {
            return Err(format!(
                "`--cfg {cfg}`: blaze passes on only a plain name or `name=\"value\"` of no cfg rustc sets"
            ));
        }
        cfgs.push(cfg.clone());
    }
    cfgs.sort_unstable();
    cfgs.dedup();
    Ok((cfgs, skipped))
}

/// [`target_cfg`] as cfgs.
pub fn target_cfgs(triple: &str, args: &CfgArgs) -> Result<Vec<Cfg>, String> {
    target_cfg(triple, args)?
        .iter()
        .map(|it| it.parse::<Cfg>().map_err(|error| format!("cfg `{it}` of target `{triple}`: {error}")))
        .collect()
}

/// The `CARGO_CFG_*` variables cargo sets for a build script's run from `cfgs`, its target's
/// [`target_cfg`] (cargo's `custom_build.rs`, `build_work`): one per name, the values of a
/// name joined by `,` in their order, `debug_assertions` left out (cargo asks rustc with no
/// profile, so it is always there and says nothing). Sorted by variable.
pub fn cargo_cfg_env(cfgs: &[String]) -> Vec<(String, String)> {
    let mut pairs: Vec<(&str, Option<&str>)> = cfgs
        .iter()
        .map(|cfg| match cfg.split_once('=') {
            Some((name, value)) => (name, Some(value.strip_prefix('"').and_then(|it| it.strip_suffix('"')).unwrap_or(value))),
            None => (cfg.as_str(), None),
        })
        .collect();
    // Stable, so a name's values keep their order.
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let mut env = Vec::with_capacity(pairs.len());
    let mut rest = pairs.as_slice();
    while let Some(&(name, _)) = rest.first() {
        let len = rest.iter().take_while(|it| it.0 == name).count();
        let (group, after) = rest.split_at(len);
        rest = after;
        if name == "debug_assertions" {
            continue;
        }
        let mut var = String::with_capacity("CARGO_CFG_".len() + name.len());
        var.push_str("CARGO_CFG_");
        var.extend(name.chars().map(|c| if c == '-' { '_' } else { c.to_ascii_uppercase() }));
        let mut value = String::new();
        for (at, it) in group.iter().filter_map(|it| it.1).enumerate() {
            if at > 0 {
                value.push(',');
            }
            value.push_str(it);
        }
        env.push((var, value));
    }
    env.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    env
}

/// What `-Zbuild-std` and a build need of a target beyond its cfgs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetFacts {
    /// The strategy of `-C panic` when none is given: `unwind` or `abort`.
    pub panic: String,
    /// Whether the target has std (its spec's `metadata.std`); without, a build asks `core`
    /// and `alloc` alone.
    pub has_std: bool,
    /// Whether the target has no libc's `memcpy` and the rest, so `-Zbuild-std` users turn on
    /// `compiler-builtins-mem` (a target without std). compiler_builtins' own build script turns
    /// its `mem` on for wasm without WASI, SGX, `-none`, nvptx, UEFI and xous whatever the
    /// features say.
    pub needs_builtins_mem: bool,
    /// `--print split-debuginfo`'s, the first the default.
    pub split_debuginfo: Vec<String>,
}

pub fn target_facts(triple: &str) -> Result<TargetFacts, String> {
    let target = target(triple)?;
    let has_std = target
        .spec()?
        .std
        .ok_or_else(|| format!("the recorded target spec of `{triple}` does not say whether it has std"))?;
    Ok(TargetFacts {
        panic: String::from(&*target.panic),
        has_std,
        needs_builtins_mem: !has_std,
        split_debuginfo: target.split_debuginfo.iter().map(|it| String::from(&**it)).collect(),
    })
}

/// The target's LLVM data layout, as the pinned nightly's target spec has it
/// (`rustc --print target-spec-json`'s `data-layout`): what the front end lays types out by and
/// the back end is given. A triple blaze has no spec of is an error naming it.
pub fn data_layout(triple: &str) -> Result<String, String> {
    let target = target(triple)?;
    match &target.spec()?.data_layout {
        Some(layout) => Ok(String::from(&**layout)),
        None => Err(format!("the recorded target spec of `{triple}` has no `data-layout`")),
    }
}

/// The spec's `has-thumb-interworking`: whether the target takes both
/// `#[instruction_set(arm::a32)]` and `#[instruction_set(arm::t32)]`.
pub fn has_thumb_interworking(triple: &str) -> Result<bool, String> {
    Ok(target(triple)?.spec()?.has_thumb_interworking)
}

/// The name of the file rustc writes for a crate `name` of `crate_type` (`bin`, `lib`, `rlib`,
/// `dylib`, `cdylib`, `staticlib`, `proc-macro`) for `triple`, as `--print file-names` prints
/// it; `None` for a crate type the target does not have.
pub fn file_name(triple: &str, crate_type: &str, name: &str) -> Result<Option<String>, String> {
    let target = target(triple)?;
    let at = target
        .file_names
        .binary_search_by(|(it, _)| (**it).cmp(crate_type))
        .map_err(|_| format!("blaze does not know rustc's file of a `{crate_type}` crate for target `{triple}`"))?;
    Ok(target.file_names[at].1.as_deref().map(|it| it.replace("___", name)))
}

/// Whether blaze has data of `triple`: an error naming it where it has none.
pub fn known(triple: &str) -> Result<(), String> {
    target(triple).map(|_| ())
}

/// `rustc --print split-debuginfo --target <triple>`'s, the first the default.
pub fn split_debuginfo(triple: &str) -> Result<&'static [Box<str>], String> {
    Ok(&*target(triple)?.split_debuginfo)
}

/// What `rustc -Z unstable-options --print target-spec-json --target <triple>` prints on
/// `host`: its own, or on a host not recorded (Windows) the one every recorded host prints.
/// `None` for a spec the hosts differ on (the -linux-gnu ones, whose own rustc links with its
/// lld), which is the build's, so not known for another host.
pub fn target_spec_json(triple: &str, host: &str) -> Result<Option<&'static str>, String> {
    let specs = &target(triple)?.spec_json;
    if let Ok(at) = specs.binary_search_by(|(it, _)| (**it).cmp(host)) {
        return Ok(Some(&*specs[at].1));
    }
    let mut each = specs.iter().map(|(_, it)| &**it);
    let Some(first) = each.next() else {
        return Ok(None);
    };
    Ok(each.all(|it| it == first).then_some(first))
}

/// rustc's warning of `-C target-feature=+<feature>` of a feature neither rustc nor LLVM has
/// of the target, as it shows it to a pipe (ending in a blank line, before its count of
/// warnings); `None` where the data has none.
pub fn unknown_feature_warning(feature: &str) -> Option<String> {
    // alloc: a warning's text, at the edge (blaze's answer to a rustc asked)
    release().unknown_feature.as_ref().map(|it| it.replace("{feature}", feature))
}

/// Every triple blaze has data of, sorted.
pub fn triples() -> impl Iterator<Item = &'static str> {
    release().targets.iter().map(|(it, _)| &**it)
}

/// The version of blaze's rustc as `rustc -V` prints it after `rustc `
/// (`1.101.0-nightly (16d1ffac1 2026-10-26)`): cargo's `-C metadata` hashes it.
pub fn rustc_version() -> &'static str {
    &release().version
}

/// What `rustc -vV` prints, on `host`.
pub fn rustc_verbose_version(host: &str) -> String {
    release().verbose.replace("{host}", host)
}

/// The commit of rust-lang/rust blaze's rustc is (`commit-hash` of `rustc -vV`): the one
/// `library/UPSTREAM` names.
pub fn rustc_commit() -> &'static str {
    release()
        .verbose
        .lines()
        .find_map(|line| line.strip_prefix("commit-hash: "))
        .expect("`rustc -vV` of blaze's release names its commit")
}
