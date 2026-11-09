//! Profile-guided code layout's inputs (docs/v2/plan/design/code-layout.md): the address map
//! of each image blaze links, and the ix-prof profiles of runs of those images, read back as
//! each image's [`Weights`] by function identity, and each link's [`Heat`] by instance key.
//!
//! A map names each function by its identity ([`blaze_codegen::backend::Described::identity`]:
//! its item, which instance of it, its type arguments), which an edit of its body and a new
//! toolchain keep, so a profile of one build lays out the next ones as their code changes; a
//! function whose identity no build since has is dropped from the heat.
//!
//! Both live in `<target-dir>/layout`. `IX_PROF=<target-dir>/layout <program>` writes
//! `<program>-<pid>-<session>.ixprof` there when the program ends: blaze starts ix-prof from `IX_PROF`
//! itself, and another program links ix-prof and calls `ix_prof::start_from_env`. While the
//! directory is there, each link writes its image's map to `maps/<image>/<build id>`, so the
//! addresses a run of that build sampled name its functions. With no directory no map is
//! written, no profile is read, and a link costs nothing more.
//!
//! A profile names the image each frame is in by build id (Mach-O's `LC_UUID`, ELF's
//! `NT_GNU_BUILD_ID`, which blaze-link writes as [`blaze_link::Image::build_id`]), at the
//! address the file gives it (the load slide taken off; a return address less one, so it is
//! inside the call), which is what [`blaze_link::Image::functions`] gives. A frame whose build
//! id no map has (another program's, a system library's, a build whose map was collected) is
//! counted, never guessed at.

use std::{
    alloc::Global,
    path::{Path, PathBuf},
    time::SystemTime,
};

use blaze_codegen::InstanceKey;
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_link::Heat;
use blaze_units::query::UnitPlan;
use ix_prof::{Profile, profile::FrameKind};

use super::{Job, artifact};

/// The directory in the target directory.
pub(crate) const DIR: &str = "layout";
/// Its directory of maps, one directory an image.
const MAPS: &str = "maps";
/// The first bytes of a map: its format and version.
const MAGIC: &[u8; 8] = b"blzlmap1";
/// A map's row: address, size, identity.
const ROW: usize = 8 + 8 + 32;
/// The maps a collection keeps of one image besides those a profile names: the latest
/// links', so a run of one of them still finds its map when its profile lands after a build
/// collected.
const KEEP: usize = 4;
const IMAGE_DOMAIN: &str = "blaze v2 layout image 1";
const KEY_DOMAIN: &str = "blaze v2 layout key 1";
const WEIGHTS_DOMAIN: &str = "blaze v2 layout weights 1";

/// A build id as an image's file holds it.
pub(crate) type BuildId = [u8; 16];
/// A function's code in a map: its address, its size, its identity.
type Row = (u64, u64, ContentId);
/// One physical frame of a sample: its build, if blaze's, and its address there.
type At = (Option<BuildId>, u64);
/// On-CPU samples by their leaf, and its caller in the same build: how many, and the CPU time
/// they stand for (each its `weight_ns`, the sampler's period when it was taken).
type Sampled = HashMap<(BuildId, u64, Option<u64>), Taken, Global>;

/// Samples counted, and the CPU time in nanoseconds they stand for.
#[derive(Clone, Copy, Default)]
struct Taken {
    samples: u64,
    weight_ns: u64,
}

/// What a target directory's `layout` holds, read once a job: each image's weights, and the
/// build ids its profiles name.
pub(crate) struct Profiles {
    dir: PathBuf,
    /// Each image's weights, by its path, sorted.
    heats: Vec<(String, Weights)>,
    /// Every build id a profile names, sorted: the maps a collection keeps.
    named: Vec<BuildId>,
    pub(crate) counts: Counts,
}

/// What reading the profiles found, which its span says.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Counts {
    /// `.ixprof` files.
    pub(crate) files: u64,
    /// Samples of a thread on a CPU.
    pub(crate) samples: u64,
    /// Of those, in code of no build a map has.
    pub(crate) unmapped: u64,
    /// Of those, in a mapped build but in no Rust function of it (native code, the linker's).
    pub(crate) outside: u64,
}

/// What the profiles say of one image, by function identity: each function's own CPU time in
/// nanoseconds (it was running) and each caller-to-callee pair's (the callee was running,
/// called from the caller), summed from the samples' `weight_ns`, so profiles taken at
/// different rates weigh by the time they stand for, not by how many samples they took. Its id covers every weight: a unit's record key and the kept image's
/// layout name the profile by it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Weights {
    id: ContentId,
    functions: Vec<(ContentId, u64)>,
    calls: Vec<(ContentId, ContentId, u64)>,
}

impl Weights {
    /// Samples in any order, summed per function and per pair.
    pub(crate) fn new(
        mut functions: Vec<(ContentId, u64)>,
        mut calls: Vec<(ContentId, ContentId, u64)>,
    ) -> Weights {
        functions.sort_unstable();
        functions.dedup_by(|it, kept| {
            it.0 == kept.0 && {
                kept.1 += it.1;
                true
            }
        });
        calls.sort_unstable();
        calls.dedup_by(|it, kept| {
            (it.0, it.1) == (kept.0, kept.1) && {
                kept.2 += it.2;
                true
            }
        });
        let mut hasher = Hasher::new(WEIGHTS_DOMAIN);
        hasher.update(&(functions.len() as u64).to_le_bytes());
        for (identity, weight) in &functions {
            hasher.id(identity).update(&weight.to_le_bytes());
        }
        hasher.update(&(calls.len() as u64).to_le_bytes());
        for (from, to, weight) in &calls {
            hasher.id(from).id(to).update(&weight.to_le_bytes());
        }
        Weights { id: hasher.finish(), functions, calls }
    }

    pub(crate) fn id(&self) -> ContentId {
        self.id
    }

    /// The heat of a build whose instances are `identities`: each weight given to every
    /// instance of its identity, one of an identity the build has no instance of dropped.
    pub(crate) fn heat(&self, identities: &[(InstanceKey, ContentId)]) -> Heat {
        // alloc: once per link with a profile, the build's instances by identity
        let mut by: Vec<(ContentId, InstanceKey)> =
            identities.iter().map(|(key, identity)| (*identity, *key)).collect();
        by.sort_unstable();
        let keys = |identity: ContentId| {
            let from = by.partition_point(|it| it.0 < identity);
            by[from..].iter().take_while(move |it| it.0 == identity).map(|it| it.1)
        };
        let functions = self
            .functions
            .iter()
            .flat_map(|&(identity, weight)| keys(identity).map(move |key| (key, weight)));
        let calls = self.calls.iter().flat_map(|&(from, to, weight)| {
            keys(from).flat_map(move |from| keys(to).map(move |to| (from, to, weight)))
        });
        Heat::new(functions, calls)
    }
}

/// The job's profiles, read the first time a unit that links asks; none when its target
/// directory has no `layout`.
pub(crate) fn profiles(job: &Job) -> Result<Option<&Profiles>, String> {
    job.layout
        .get_or_init(|| {
            let target = Path::new(&job.units().target_dir);
            let span = tracing::info_span!(
                target: "blaze",
                "layout.profiles",
                dir = %target.join(DIR).display(),
                files = tracing::field::Empty,
                samples = tracing::field::Empty,
                unmapped = tracing::field::Empty,
                outside = tracing::field::Empty,
                images = tracing::field::Empty,
            );
            let _entered = span.enter();
            let read = Profiles::read(target);
            if let Ok(Some(read)) = &read {
                let Counts { files, samples, unmapped, outside } = read.counts;
                span.record("files", files)
                    .record("samples", samples)
                    .record("unmapped", unmapped)
                    .record("outside", outside)
                    .record("images", read.heats.len() as u64);
            }
            read
        })
        .as_ref()
        .map(Option::as_ref)
        .map_err(Clone::clone)
}

/// What `plan`'s record key takes of the job's profiles: whether its links write maps, and
/// the heat of each image it links, by path. None when the target directory has no `layout`.
pub(crate) fn key(job: &Job, plan: &UnitPlan) -> Result<Option<ContentId>, String> {
    Ok(profiles(job)?.map(|profiles| profiles.key(artifact::made(plan))))
}

impl Profiles {
    /// The profiles in `target`'s `layout`; none when there is no such directory.
    pub(crate) fn read(target: &Path) -> Result<Option<Profiles>, String> {
        let dir = target.join(DIR);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("{}: {error}", dir.display())),
        };
        // alloc: once per job, a path a profile
        let mut files = Vec::new();
        for entry in entries {
            let path = entry.map_err(|error| format!("{}: {error}", dir.display()))?.path();
            if path.extension().is_some_and(|it| it == "ixprof") {
                files.push(path);
            }
        }
        files.sort_unstable();
        let mut counts = Counts { files: files.len() as u64, ..Counts::default() };
        // alloc: once per job, a distinct leaf and caller sampled
        let mut sampled: Sampled = HashMap::new_in(Global);
        for file in &files {
            let bytes =
                std::fs::read(file).map_err(|error| format!("{}: {error}", file.display()))?;
            let profile = Profile::from_ixprof(&bytes).map_err(|error| {
                format!("{}: {error}; remove it, or profile again", file.display())
            })?;
            samples(&profile, &mut sampled, &mut counts)
                .map_err(|why| format!("{}: {why}", file.display()))?;
        }
        // alloc: once per job, a build id a profile names
        let mut named: Vec<BuildId> = sampled.keys().map(|it| it.0).collect();
        named.sort_unstable();
        named.dedup();
        let maps = maps(&dir, &named)?;
        // alloc: once per job, an image a profile ran
        let mut images: Vec<(String, Vec<(ContentId, u64)>, Vec<(ContentId, ContentId, u64)>)> =
            Vec::new();
        for (&(build, leaf, caller), &Taken { samples, weight_ns }) in &sampled {
            let of = maps.iter().filter(|it| it.0 == build).map(|it| (&it.1, &it.2));
            let mut found = false;
            for (path, rows) in of {
                let Some(callee) = function(rows, leaf) else { continue };
                found = true;
                let at = match images.iter().position(|it| it.0 == *path) {
                    Some(at) => at,
                    None => {
                        // clone: once per image a profile ran
                        images.push((path.clone(), Vec::new(), Vec::new()));
                        images.len() - 1
                    }
                };
                images[at].1.push((callee, weight_ns));
                if let Some(caller) = caller.and_then(|it| function(rows, it)) {
                    images[at].2.push((caller, callee, weight_ns));
                }
            }
            if !found {
                match maps.iter().any(|it| it.0 == build) {
                    true => counts.outside += samples,
                    false => counts.unmapped += samples,
                }
            }
        }
        let mut heats: Vec<(String, Weights)> = images
            .into_iter()
            .map(|(path, functions, calls)| (path, Weights::new(functions, calls)))
            .collect();
        heats.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(Some(Profiles { dir, heats, named, counts }))
    }

    /// The weights of the image at `path`, when a profile ran it.
    pub(crate) fn weights(&self, path: &str) -> Option<&Weights> {
        self.heats.binary_search_by(|it| it.0.as_str().cmp(path)).ok().map(|at| &self.heats[at].1)
    }

    /// The key input of a unit linking the images at `paths`: that its links write maps,
    /// and each one's heat.
    fn key<'p>(&self, paths: impl IntoIterator<Item = &'p str>) -> ContentId {
        let mut hasher = Hasher::new(KEY_DOMAIN);
        for path in paths {
            if let Some(weights) = self.weights(path) {
                hasher.part(path.as_bytes()).id(&weights.id);
            }
        }
        hasher.finish()
    }

    /// Writes the address map of the image at `path` whose build id is `id`, its `functions`
    /// named by `identities` (the build's), then removes that image's maps no profile names
    /// but the newest [`KEEP`].
    pub(crate) fn write_map(
        &self,
        path: &str,
        id: BuildId,
        functions: &[(InstanceKey, u64, u64)],
        identities: &[(InstanceKey, ContentId)],
    ) -> Result<(), String> {
        // alloc: once per link that keeps a map, the build's identities by key
        let mut by_key = identities.to_vec();
        by_key.sort_unstable();
        let dir = self.dir.join(MAPS).join(image_dir(path));
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("making {}: {error}", dir.display()))?;
        // A function of no code holds no address, and one at its address would hide the
        // function that does from `function`.
        let sized = || functions.iter().filter(|it| it.2 > 0);
        let rows = sized().count();
        // alloc: once per link that keeps a map, the map
        let mut bytes = Vec::with_capacity(MAGIC.len() + 4 + path.len() + 8 + rows * ROW);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
        bytes.extend_from_slice(path.as_bytes());
        bytes.extend_from_slice(&(rows as u64).to_le_bytes());
        for (key, addr, size) in sized() {
            let at = by_key.binary_search_by(|it| it.0.cmp(key)).map_err(|_| {
                format!(
                    "the address map of {path}: its function {:x} is no instance the build reached",
                    key.0
                )
            })?;
            bytes.extend_from_slice(&addr.to_le_bytes());
            bytes.extend_from_slice(&size.to_le_bytes());
            bytes.extend_from_slice(&by_key[at].1.0);
        }
        let name = hex(&id);
        let file = dir.join(&name);
        // One build id, one map: a link that gave another image's id to other code is refused,
        // never written over (the id is of the image's lineage and generation, which two
        // daemons can repeat for two different incremental links).
        match std::fs::read(&file) {
            Ok(kept) if kept == bytes => return self.collect(&dir, id),
            Ok(_) => {
                return Err(format!(
                    "the address map {} names other functions for this link's build id {name}: two links of \
                     {path} gave one build id to different code; remove {} to profile it afresh",
                    file.display(),
                    dir.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", file.display())),
        }
        // Whole or not at all: a profile read while it is written finds the last one.
        let part = dir.join(format!("{name}.part-{}", std::process::id()));
        std::fs::write(&part, &bytes)
            .map_err(|error| format!("writing {}: {error}", part.display()))?;
        std::fs::rename(&part, &file).map_err(|error| {
            format!("renaming {} to {}: {error}", part.display(), file.display())
        })?;
        self.collect(&dir, id)
    }

    /// Removes the maps in `dir` (one image's) that no profile names, but the newest [`KEEP`]
    /// counting `written`.
    fn collect(&self, dir: &Path, written: BuildId) -> Result<(), String> {
        let entries =
            std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        // alloc: once per link that keeps a map, a map of its image's
        let mut unnamed: Vec<(SystemTime, PathBuf)> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
            let Some(id) = entry.file_name().to_str().and_then(build_id) else { continue };
            if id == written || self.named.binary_search(&id).is_ok() {
                continue;
            }
            let at = entry.path();
            // Gone: another job's collection of this image took it first.
            let modified = match entry.metadata().and_then(|it| it.modified()) {
                Ok(modified) => modified,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("{}: {error}", at.display())),
            };
            unnamed.push((modified, at));
        }
        unnamed.sort_unstable_by(|a, b| b.cmp(a));
        for (_, at) in unnamed.iter().skip(KEEP - 1) {
            match std::fs::remove_file(at) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("removing {}: {error}", at.display())),
            }
        }
        Ok(())
    }
}

/// Counts each on-CPU sample of `profile` into `sampled` by its leaf, and its caller when in
/// the same build: each by its physical frame, the inlined frames at one address one frame,
/// weighed by its `weight_ns`.
fn samples(profile: &Profile, sampled: &mut Sampled, counts: &mut Counts) -> Result<(), String> {
    for sample in profile.cpu_samples.iter().filter(|it| it.on_cpu) {
        counts.samples += 1;
        let [leaf, caller] = physical(profile, sample.stack)?;
        let Some((Some(build), leaf)) = leaf else {
            counts.unmapped += 1;
            continue;
        };
        let caller = caller.and_then(|(of, at)| (of == Some(build)).then_some(at));
        let taken = sampled.entry((build, leaf, caller)).or_default();
        taken.samples += 1;
        taken.weight_ns = taken.weight_ns.saturating_add(sample.weight_ns);
    }
    Ok(())
}

/// The innermost two physical frames of stack node `stack`: frames at one address of one
/// build are one machine frame (the inlined functions in it), and span frames are skipped.
fn physical(profile: &Profile, stack: u32) -> Result<[Option<At>; 2], String> {
    let mut out = [None, None];
    let mut found = 0;
    let mut node = Some(stack);
    // A stack is a path to the trie's root: no longer than the trie.
    let mut fuel = profile.stacks.len();
    while let Some(at) = node
        && found < 2
    {
        fuel = fuel.checked_sub(1).ok_or_else(|| format!("stack {stack} has a cycle"))?;
        let it = profile
            .stacks
            .get(at as usize)
            .ok_or_else(|| format!("stack {stack} names node {at}, past the table"))?;
        let frame = profile
            .frames
            .get(it.frame as usize)
            .ok_or_else(|| format!("stack node {at} names frame {}, past the table", it.frame))?;
        node = it.parent;
        if frame.kind != FrameKind::Native {
            continue;
        }
        // A frame with no address is code no map can name: no build.
        let here = match frame.address {
            Some(address) => (frame.build_id.as_deref().and_then(build_id), address),
            None => (None, 0),
        };
        if found > 0 && out[found - 1] == Some(here) {
            continue;
        }
        out[found] = Some(here);
        found += 1;
    }
    Ok(out)
}

/// The maps of each of `named` in `dir`: (build id, image path, rows by address).
fn maps(dir: &Path, named: &[BuildId]) -> Result<Vec<(BuildId, String, Vec<Row>)>, String> {
    let root = dir.join(MAPS);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", root.display())),
    };
    // alloc: once per job, an image a map is kept of
    let mut images = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", root.display()))?;
        // An image's maps are a directory; anything else here (a `.DS_Store`) is none of ours.
        if entry
            .file_type()
            .map_err(|error| format!("{}: {error}", entry.path().display()))?
            .is_dir()
        {
            images.push(entry.path());
        }
    }
    images.sort_unstable();
    // alloc: once per job, a map a profile names
    let mut out = Vec::new();
    for image in &images {
        for id in named {
            let file = image.join(hex(id));
            let bytes = match std::fs::read(&file) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("{}: {error}", file.display())),
            };
            let (path, rows) =
                parse(&bytes).map_err(|why| format!("{}: {why}; remove it", file.display()))?;
            out.push((*id, path, rows));
        }
    }
    Ok(out)
}

/// A map's image path and rows.
fn parse(bytes: &[u8]) -> Result<(String, Vec<Row>), String> {
    let rest = bytes.strip_prefix(MAGIC).ok_or("not an address map of this blaze")?;
    let (len, rest) = rest.split_first_chunk::<4>().ok_or("cut short in its path's length")?;
    let (path, rest) =
        rest.split_at_checked(u32::from_le_bytes(*len) as usize).ok_or("cut short in its path")?;
    let path = std::str::from_utf8(path).map_err(|_| "its path is not UTF-8")?;
    let (count, rest) = rest.split_first_chunk::<8>().ok_or("cut short in its count of rows")?;
    let count = u64::from_le_bytes(*count);
    if rest.len() as u64 != count.saturating_mul(ROW as u64) {
        return Err(format!("{count} rows of {ROW} bytes, and {} bytes", rest.len()));
    }
    // alloc: once per job, a map a profile names
    let rows: Vec<Row> = rest
        .chunks_exact(ROW)
        .map(|row| {
            let word =
                |at: usize| u64::from_le_bytes(row[at..at + 8].try_into().unwrap_or_default());
            let mut identity = [0; 32];
            identity.copy_from_slice(&row[16..]);
            (word(0), word(8), ContentId(identity))
        })
        .collect();
    if !rows.is_sorted_by_key(|it| it.0) {
        return Err("its rows are not by address".to_owned());
    }
    // clone: once per job, a map a profile names
    Ok((path.to_owned(), rows))
}

/// The identity of the function whose code holds `address`.
fn function(rows: &[Row], address: u64) -> Option<ContentId> {
    let at = rows.partition_point(|it| it.0 <= address).checked_sub(1)?;
    let (start, size, key) = rows[at];
    (address - start < size).then_some(key)
}

/// The directory of the maps of the image at `path`: its file name, then a hash of the whole
/// path, so two images of one name keep two.
fn image_dir(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let id = ContentId::of(IMAGE_DOMAIN, path.as_bytes());
    format!("{name}-{}", hex(&id.0[..8]))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    // alloc: a file name
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A build id as ix-prof writes it, hex; none for one blaze did not write (another length).
fn build_id(text: &str) -> Option<BuildId> {
    let bytes = text.as_bytes();
    if bytes.len() != 32 {
        return None;
    }
    let digit = |it: u8| (it as char).to_digit(16).map(|it| it as u8);
    let mut out = [0; 16];
    for (at, pair) in bytes.chunks_exact(2).enumerate() {
        out[at] = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests;
