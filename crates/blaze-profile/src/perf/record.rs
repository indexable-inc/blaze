//! A run's `.ixprof` repetitions, read through the code maps of the builds that ran, as rows
//! ([`super::rows`]).
//!
//! Each on-CPU sample's stack is read innermost first, as physical frames: consecutive frames
//! at one (build id, address) are one place in the machine code, the inlined functions there
//! included. Each physical frame resolves through the code map of its build
//! ([`super::map`]) to the node whose code holds the address, which names it by path and by
//! identity. A frame no map names keeps the frame's own symbol and module, with the reason as
//! its `gap`: no build id, no code map kept of its build, or an address outside blaze's
//! functions in a mapped image. Nothing is guessed from a name.
//!
//! A function's self samples are those with it running (the innermost physical frame); its
//! total samples those with it anywhere on the stack, once a sample however often it recurs.
//! Instances of one identity (built with two settings) merge into one row: its name the
//! smallest of theirs, its `key`, `code` and `deep` a hash of their sorted set when they
//! differ.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use blaze_ids::{ContentId, Hasher};
use ix_prof::{Profile, profile::FrameKind};

use super::{
    Id, hex,
    map::{self, Map},
    rows::{FORMAT, FnRow, RunRow},
};

const REP_DOMAIN: &str = "blaze v2 perf rep 1";
const RUN_DOMAIN: &str = "blaze v2 perf run 1";
const MERGED_DOMAIN: &str = "blaze v2 perf merged 1";

/// What a run is recorded with besides its profiles.
#[derive(Clone, Debug)]
pub struct Inputs {
    /// `<target-dir>/perf`: where the links kept their code maps.
    pub perf: PathBuf,
    /// The digest of the workspace's source tree, or why there is none.
    pub tree: Result<String, String>,
    /// The BLAKE3 hash of the blaze recording it, hex.
    pub blaze: String,
    /// The machine the run ran on.
    pub host: String,
}

/// A map read for a build id, with what resolving addresses needs.
struct Loaded {
    map: Map,
    by_address: Vec<u32>,
    deep: Vec<Id>,
}

/// What a physical frame names: a node of a map by identity, or a frame no map names.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Name {
    Identity(Id),
    Named { module: Option<String>, function: String },
}

/// A function's rows being added up.
struct Function {
    names: BTreeSet<String>,
    module: Option<String>,
    keys: BTreeSet<Id>,
    codes: BTreeSet<Id>,
    deeps: BTreeSet<Id>,
    gap: Option<String>,
    /// Per repetition.
    self_samples: Vec<u64>,
    total_samples: Vec<u64>,
}

/// Reads `reps`, the `.ixprof` bytes of each repetition of one run, through the code maps in
/// `inputs.perf`, as the run's row and its function rows. Repetitions of different programs,
/// or of different machines' architectures, are refused.
pub fn record(reps: &[Vec<u8>], inputs: &Inputs) -> Result<(RunRow, Vec<FnRow>), String> {
    if reps.is_empty() {
        // alloc: an error
        return Err("a run needs at least one profile".to_owned());
    }
    // alloc: once a run, its repetitions
    let mut profiles = Vec::with_capacity(reps.len());
    for (at, bytes) in reps.iter().enumerate() {
        let profile =
            Profile::from_ixprof(bytes).map_err(|error| format!("repetition {at}: {error}"))?;
        profiles.push(profile);
    }
    let first = &profiles[0].meta;
    for (at, it) in profiles.iter().enumerate().skip(1) {
        if it.meta.program != first.program || it.meta.arch != first.arch || it.meta.os != first.os
        {
            // alloc: an error
            return Err(format!(
                "repetition {at} is of {} on {}-{}, and repetition 0 of {} on {}-{}: a run is one program on one machine",
                it.meta.program, it.meta.arch, it.meta.os, first.program, first.arch, first.os
            ));
        }
    }
    // The run's name: its repetitions' contents, in any order.
    // alloc: once a run
    let mut ids: Vec<ContentId> = reps.iter().map(|it| ContentId::of(REP_DOMAIN, it)).collect();
    ids.sort_unstable();
    let mut hasher = Hasher::new(RUN_DOMAIN);
    for id in &ids {
        hasher.id(id);
    }
    let run = hex(&hasher.finish().0[..8]);

    // alloc: once a run, its gaps
    let mut gaps: BTreeSet<String> = BTreeSet::new();
    if let Err(why) = &inputs.tree {
        gaps.insert(format!("tree: {why}"));
    }
    let period = first.cpu_period_ns;
    match period {
        None => {
            gaps.insert(
                "cpu_period_ns: the sampler was off, so its samples weigh nothing".to_owned(),
            );
        }
        Some(period) => {
            for (at, it) in profiles.iter().enumerate() {
                if it.meta.cpu_period_ns != Some(period) {
                    gaps.insert(format!(
                        "cpu_period_ns: repetition {at} sampled every {:?} ns, repetition 0 every {period} ns",
                        it.meta.cpu_period_ns
                    ));
                }
            }
        }
    }
    gaps.insert("calls: not recorded (no instrumented build)".to_owned());
    gaps.insert("instructions: not recorded (no counting sampler)".to_owned());

    // alloc: once a run, the maps by build id
    let mut maps: BTreeMap<String, Option<Loaded>> = BTreeMap::new();
    // alloc: once a run, its functions
    let mut slots: BTreeMap<Name, Function> = BTreeMap::new();
    let mut samples = 0u64;
    let mut unmapped = 0u64;
    let mut wall_ns = 0i64;
    let mut started = i64::MAX;
    // The executable: the module whose file name is the program's.
    let mut binary: Option<(String, Option<String>)> = None;
    let reps_count = profiles.len();
    for (rep, profile) in profiles.iter().enumerate() {
        wall_ns = wall_ns.saturating_add(profile.meta.end_ns.saturating_sub(profile.meta.start_ns));
        started = started.min(profile.meta.start_unix_ns);
        // Each frame's place, resolved once a repetition.
        // alloc: once a repetition, per frame
        let mut resolved: Vec<Option<Name>> = vec![None; profile.frames.len()];
        // alloc: once a repetition, reused per sample
        let mut on_stack: Vec<Name> = Vec::new();
        for sample in &profile.cpu_samples {
            if !sample.on_cpu {
                continue;
            }
            samples += 1;
            on_stack.clear();
            let mut node = Some(sample.stack);
            let mut last: Option<(Option<&str>, u64)> = None;
            // A stack is a path to the trie's root: no longer than the trie.
            let mut fuel = profile.stacks.len();
            let mut leaf = true;
            let mut leaf_mapped = false;
            while let Some(at) = node {
                fuel = fuel.checked_sub(1).ok_or_else(|| {
                    format!("repetition {rep}: stack {} has a cycle", sample.stack)
                })?;
                let it = profile.stacks.get(at as usize).ok_or_else(|| {
                    format!(
                        "repetition {rep}: stack {} names node {at}, past the table",
                        sample.stack
                    )
                })?;
                node = it.parent;
                let frame = profile.frames.get(it.frame as usize).ok_or_else(|| {
                    format!(
                        "repetition {rep}: stack node {at} names frame {}, past the table",
                        it.frame
                    )
                })?;
                if frame.kind != FrameKind::Native {
                    continue;
                }
                // One place in the machine code: the inlined frames at it are the same place.
                if let Some(address) = frame.address {
                    let here = (frame.build_id.as_deref(), address);
                    if last == Some(here) {
                        continue;
                    }
                    last = Some(here);
                }
                if binary.is_none()
                    && let Some(module) = &frame.module
                    && Path::new(module)
                        .file_name()
                        .is_some_and(|it| it.to_str() == Some(first.program.as_str()))
                {
                    // clone: once a run, the executable's path and build id
                    binary = Some((module.clone(), frame.build_id.clone()));
                }
                let name = match &resolved[it.frame as usize] {
                    // clone: a resolved name, per frame of a sample
                    Some(name) => name.clone(),
                    None => {
                        let name = resolve(
                            profile,
                            it.frame,
                            &inputs.perf,
                            &mut maps,
                            &mut slots,
                            &mut gaps,
                            reps_count,
                        )?;
                        // clone: once per frame of a repetition
                        resolved[it.frame as usize] = Some(name.clone());
                        name
                    }
                };
                if leaf {
                    leaf_mapped = matches!(name, Name::Identity(_));
                    if let Some(function) = slots.get_mut(&name) {
                        function.self_samples[rep] += 1;
                    }
                    leaf = false;
                }
                on_stack.push(name);
            }
            if !leaf_mapped {
                unmapped += 1;
            }
            on_stack.sort_unstable();
            on_stack.dedup();
            for name in &on_stack {
                if let Some(function) = slots.get_mut(name) {
                    function.total_samples[rep] += 1;
                }
            }
        }
    }

    // The executable's hash, as it is now, unless it was written after the run began: then
    // what ran is gone, and that is the gap.
    let (binary_hash, build_id) = match &binary {
        None => {
            gaps.insert(format!("binary: no sampled frame is in {}", first.program));
            (None, None)
        }
        Some((path, build_id)) => {
            (hash_binary(Path::new(path), started, &mut gaps), build_id.clone())
        }
    };
    // alloc: once a run, its rows
    let mut rows = Vec::new();
    for (name, function) in &slots {
        let (identity, function_name) = match name {
            Name::Identity(id) => {
                (Some(hex(id)), function.names.first().cloned().unwrap_or_default())
            }
            // clone: once a row
            Name::Named { function, .. } => (None, function.clone()),
        };
        let one = |set: &BTreeSet<Id>| match set.len() {
            0 => None,
            1 => set.first().map(|it| hex(it)),
            _ => {
                let mut hasher = Hasher::new(MERGED_DOMAIN);
                hasher.update(&(set.len() as u64).to_le_bytes());
                for it in set {
                    hasher.update(it);
                }
                Some(hex(&hasher.finish().0))
            }
        };
        let (key, code, deep) = (one(&function.keys), one(&function.codes), one(&function.deeps));
        for rep in 0..reps_count {
            if function.total_samples[rep] == 0 {
                continue;
            }
            rows.push(FnRow {
                // clone: once a row
                run: run.clone(),
                rep: rep as u32,
                // clone: once a row
                function: function_name.clone(),
                // clone: once a row
                module: function.module.clone(),
                // clone: once a row
                identity: identity.clone(),
                // clone: once a row
                code: code.clone(),
                // clone: once a row
                key: key.clone(),
                // clone: once a row
                deep: deep.clone(),
                self_samples: function.self_samples[rep],
                total_samples: function.total_samples[rep],
                calls: None,
                instructions: None,
                // clone: once a row
                gap: function.gap.clone(),
            });
        }
    }
    let used_maps = maps.values().any(Option::is_some);
    let row = RunRow {
        // clone: once a run
        run: run.clone(),
        format: FORMAT,
        // clone: once a run
        target: first.program.clone(),
        started_unix_ns: started,
        reps: reps_count as u32,
        tree: inputs.tree.as_ref().ok().cloned(),
        // clone: once a run
        blaze: inputs.blaze.clone(),
        binary: binary_hash,
        build_id,
        // clone: once a run
        host: inputs.host.clone(),
        // clone: once a run
        arch: first.arch.clone(),
        // clone: once a run
        os: first.os.clone(),
        cpu_period_ns: period,
        // clone: once a run
        sampler: first.cpu_sampler.clone(),
        samples,
        unmapped,
        wall_ns,
        maps: used_maps.then(|| inputs.perf.join(map::MAPS).display().to_string()),
        // alloc: once a run
        gaps: (!gaps.is_empty()).then(|| gaps.into_iter().collect::<Vec<_>>().join("; ")),
    };
    Ok((row, rows))
}

/// The function the physical frame `frame` of `profile` is, added to `slots` if new.
fn resolve(
    profile: &Profile,
    frame: u32,
    perf: &Path,
    maps: &mut BTreeMap<String, Option<Loaded>>,
    slots: &mut BTreeMap<Name, Function>,
    gaps: &mut BTreeSet<String>,
    reps: usize,
) -> Result<Name, String> {
    let it = &profile.frames[frame as usize];
    let module = it.module.as_deref();
    let shown = module.map_or("an unknown image", |it| it);
    let named = |gap: String| {
        let name = Name::Named {
            // alloc: once a frame
            module: module.map(str::to_owned),
            // clone: once a frame
            function: it.function.clone(),
        };
        (name, gap)
    };
    let (name, gap, node) = match (it.address, it.build_id.as_deref()) {
        (None, _) => {
            let (name, gap) = named("the frame has no address".to_owned());
            (name, Some(gap), None)
        }
        (Some(_), None) => {
            let (name, gap) = named(format!("{shown} has no build id"));
            (name, Some(gap), None)
        }
        (Some(address), Some(build)) => {
            // alloc: once a build id
            let build = build.to_ascii_lowercase();
            if !maps.contains_key(&build) {
                let loaded = match map::read(perf, &build)? {
                    None => {
                        gaps.insert(format!("no code map for build {build} ({shown})"));
                        None
                    }
                    Some((_, map)) => {
                        let by_address = map.by_address();
                        let deep = map.deep()?;
                        Some(Loaded { map, by_address, deep })
                    }
                };
                // clone: once a build id
                maps.insert(build.clone(), loaded);
            }
            match &maps[&build] {
                None => {
                    let (name, gap) = named(format!("no code map for build {build}"));
                    (name, Some(gap), None)
                }
                Some(loaded) => match loaded.map.function_at(&loaded.by_address, address) {
                    None => {
                        let (name, gap) = named(format!("outside blaze's functions in {shown}"));
                        (name, Some(gap), None)
                    }
                    Some(node) => match loaded.map.nodes[node as usize].identity {
                        Some(identity) => (Name::Identity(identity), None, Some((loaded, node))),
                        None => {
                            let (name, gap) = named("its code map gives it no identity".to_owned());
                            (name, Some(gap), None)
                        }
                    },
                },
            }
        }
    };
    // clone: once a frame, the slot's key
    let slot = slots.entry(name.clone()).or_insert_with(|| Function {
        names: BTreeSet::new(),
        // alloc: once a function
        module: module.map(str::to_owned),
        keys: BTreeSet::new(),
        codes: BTreeSet::new(),
        deeps: BTreeSet::new(),
        gap,
        // alloc: once a function
        self_samples: vec![0; reps],
        // alloc: once a function
        total_samples: vec![0; reps],
    });
    if let Some((loaded, node)) = node {
        let it = &loaded.map.nodes[node as usize];
        // clone: once a frame, a name of an instance
        slot.names.insert(it.name.clone());
        slot.keys.insert(it.key);
        slot.codes.insert(it.code);
        slot.deeps.insert(loaded.deep[node as usize]);
    }
    Ok(name)
}

/// The BLAKE3 hash of the executable at `path`, hex; none, with the gap named, when it can't
/// be read or was written after the run began at `started_unix_ns`.
fn hash_binary(path: &Path, started_unix_ns: i64, gaps: &mut BTreeSet<String>) -> Option<String> {
    let modified = match std::fs::metadata(path).and_then(|it| it.modified()) {
        Ok(modified) => modified,
        Err(error) => {
            gaps.insert(format!("binary: {}: {error}", path.display()));
            return None;
        }
    };
    let modified_ns = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| i64::try_from(it.as_nanos()).unwrap_or(i64::MAX));
    if modified_ns > started_unix_ns {
        gaps.insert(format!(
            "binary: {} was written after the run began, so what ran is gone",
            path.display()
        ));
        return None;
    }
    match blaze_v2_proof::tree::hash_file(path) {
        Ok((_, hash)) => Some(hex(&hash)),
        Err(error) => {
            gaps.insert(format!("binary: {}: {error}", path.display()));
            None
        }
    }
}
