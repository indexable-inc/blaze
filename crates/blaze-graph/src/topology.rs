//! Which CPUs share what (docs/v2/plan/lanes/core.md section 4, docs/v2/plan/lanes/core/plane.md "The executor"), for the
//! worker pool's groups and the order its workers steal in.
//!
//! A **group** is the CPUs that share a last-level cache (an EPYC's CCD: the level 3 cache's
//! `shared_cpu_list`) or, where the machine says so, a cluster (`cluster_cpus_list`, a
//! Graviton's); where both are said, the smaller wins. A cluster that is one core (x86 says its
//! SMT siblings are a cluster) says nothing. A group never spans NUMA nodes (`cpuM/nodeN`):
//! where a cache is shared across nodes (Intel's sub-NUMA clusters), each node's part of it is
//! a group, so the memory a group's workers allocate is all local. On macOS a group is an L2
//! cluster of one performance level, the CPUs of level 0 are `fast`, and there is one node. A
//! machine that says nothing is one group on one socket and node. Only online CPUs in the
//! process's affinity mask are counted.
//!
//! Group ids are dense from 0 and stable for a machine: sorted by their first CPU.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

use blaze_hash::HashMap;
use blaze_radix as radix;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cpu {
    /// The CPU's number: the kernel's on Linux (what affinity takes), the order of the
    /// performance levels on macOS.
    pub id: u32,
    pub group: u32,
    pub socket: u32,
    /// Its NUMA node: the `nodeN` entry of `/sys/devices/system/cpu/cpuM/`; 0 where the
    /// machine has none, and on macOS.
    pub node: u32,
    /// Of the fastest cores: Apple's performance level 0, or the largest `cpu_capacity` of an
    /// Arm big.LITTLE machine; every CPU where the machine says nothing.
    pub fast: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)] // clone: once per pool started, kept by it
pub struct Topology {
    /// By id.
    pub cpus: Vec<Cpu>,
}

impl Topology {
    /// This machine's, as far as the process may use it.
    ///
    /// # Errors
    ///
    /// Where the kernel does not say what the pool is shaped by (sysfs, the affinity mask, the
    /// performance levels), naming what: a pool shaped by a guess is never made.
    pub fn detect() -> io::Result<Topology> {
        detect()
    }

    /// `count` CPUs numbered from 0, one group on one socket and node: what a machine that
    /// says nothing is, and the shape of a pool whose threads are not bound to CPUs.
    pub fn flat(count: usize) -> Topology {
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        Topology {
            // alloc: the pool's shape, once per pool
            cpus: (0..count).map(|id| Cpu { id, group: 0, socket: 0, node: 0, fast: true }).collect(),
        }
    }

    /// The topology `root` says, where `root` is the kernel's `/sys/devices/system/cpu` (or a
    /// tree laid out as it is, for tests). The affinity mask is not applied here.
    pub fn from_sysfs(root: &Path) -> io::Result<Topology> {
        let online = list(&read(&root.join("online"))?)
            .map_err(|why| at(&root.join("online"), &why))?;
        let mut found = Vec::with_capacity(online.len());
        let mut capacities: HashMap<u32, u64> = HashMap::default();
        for &id in &online {
            // alloc: a sysfs path, once per CPU per pool
            let cpu = root.join(format!("cpu{id}"));
            let cache = last_level(&cpu)?.map(|it| &it & &online);
            let cluster = cluster(&cpu, id, &online)?;
            let mut share = match (cache, cluster) {
                (Some(cache), Some(cluster)) if cluster.len() < cache.len() => cluster,
                (Some(cache), _) => cache,
                (None, Some(cluster)) => cluster,
                // clone: once per CPU per pool, setup
                (None, None) => online.clone(),
            };
            share.insert(id);
            let socket = match read_opt(&cpu.join("topology/physical_package_id"))? {
                Some(text) => text.trim().parse::<i64>().ok().and_then(|it| u32::try_from(it).ok()),
                None => None,
            };
            if let Some(text) = read_opt(&cpu.join("cpu_capacity"))?
                && let Ok(capacity) = text.trim().parse::<u64>()
            {
                capacities.insert(id, capacity);
            }
            found.push((id, share, socket.unwrap_or(0), node_of(&cpu)?));
        }
        let node: HashMap<u32, u32> =
            // alloc: once per pool, setup
            found.iter().map(|&(id, _, _, node)| (id, node)).collect();
        let fastest = capacities.values().copied().max();
        // Groups are numbered in the order they are first met, so the map's order is never read.
        let mut sets: HashMap<Vec<u32>, u32> = HashMap::default();
        let mut cpus = Vec::with_capacity(found.len());
        for (id, share, socket, home) in found {
            // A cache shared across nodes is split by node: a group's memory is all local.
            let share: Vec<u32> =
                // alloc: a group's key, once per CPU per pool
                share.into_iter().filter(|it| node.get(it) == Some(&home)).collect();
            let next = u32::try_from(sets.len()).unwrap_or(u32::MAX);
            let group = *sets.entry(share).or_insert(next);
            let fast = match (capacities.get(&id), fastest) {
                (Some(capacity), Some(fastest)) => *capacity == fastest,
                _ => true,
            };
            cpus.push(Cpu { id, group, socket, node: home, fast });
        }
        Ok(dense(cpus))
    }

    /// macOS's shape: for each performance level from the fastest, its CPUs and how many of
    /// them share an L2 (`hw.perflevel{N}.logicalcpu`, `hw.perflevel{N}.cpusperl2`).
    pub fn from_perflevels(levels: &[(u32, u32)]) -> Topology {
        // alloc: the pool's shape, once per pool
        let (mut cpus, mut id, mut first_group) = (Vec::new(), 0u32, 0u32);
        for (level, &(logical, per_l2)) in levels.iter().enumerate() {
            let per_l2 = if per_l2 == 0 { logical.max(1) } else { per_l2 };
            for at in 0..logical {
                let group = first_group + at / per_l2;
                cpus.push(Cpu { id, group, socket: 0, node: 0, fast: level == 0 });
                id += 1;
            }
            first_group += logical.div_ceil(per_l2);
        }
        dense(cpus)
    }

    /// Only the CPUs of `allowed`, their groups numbered again.
    pub fn only(&self, allowed: &BTreeSet<u32>) -> Topology {
        // alloc: the pool's shape, once per pool
        dense(self.cpus.iter().copied().filter(|it| allowed.contains(&it.id)).collect())
    }

    pub fn groups(&self) -> u32 {
        self.cpus.iter().map(|it| it.group + 1).max().unwrap_or(0)
    }

    /// The groups a worker of `group` takes work from, in order: its own, then the others of
    /// its NUMA node, then of its socket, then the rest, each nearest by id first (ties to the
    /// lower). Empty for a group the topology does not have.
    pub fn steal_order(&self, group: u32) -> Vec<u32> {
        let mut place_of: HashMap<u32, (u32, u32)> = HashMap::default();
        for cpu in &self.cpus {
            place_of.entry(cpu.group).or_insert((cpu.socket, cpu.node));
        }
        // alloc: empty, allocates nothing
        let Some(&(socket, node)) = place_of.get(&group) else { return Vec::new() };
        let mut rest: Vec<(u32, (u32, u32))> =
            // alloc: a worker's steal order, once per worker per pool
            place_of.into_iter().filter(|&(it, _)| it != group).collect();
        // The key ends in the group id, which is unique: a total order, whatever the map's.
        radix::sort_copy_by_key(&mut rest, |&(it, (its_socket, its_node))| {
            (u128::from(its_socket != socket) << 65)
                | (u128::from(its_node != node) << 64)
                | u128::from(radix::pack_32(it.abs_diff(group), it))
        });
        let rest = rest.into_iter().map(|(it, _)| it);
        // alloc: a worker's steal order, once per worker per pool
        std::iter::once(group).chain(rest).collect()
    }

    /// The NUMA node of each group, by group id.
    pub fn nodes(&self) -> Vec<u32> {
        // alloc: once per pool, setup
        let mut node_of = vec![0; self.groups() as usize];
        for cpu in &self.cpus {
            node_of[cpu.group as usize] = cpu.node;
        }
        node_of
    }
}

/// `cpus` by id, their groups numbered from 0 in the order of their first CPU.
fn dense(mut cpus: Vec<Cpu>) -> Topology {
    radix::sort_copy_by_key(&mut cpus, |it| it.id);
    let mut renamed: HashMap<u32, u32> = HashMap::default();
    for cpu in &mut cpus {
        let next = u32::try_from(renamed.len()).unwrap_or(u32::MAX);
        cpu.group = *renamed.entry(cpu.group).or_insert(next);
    }
    Topology { cpus }
}

#[cfg(target_os = "linux")]
fn detect() -> io::Result<Topology> {
    let topology = Topology::from_sysfs(Path::new("/sys/devices/system/cpu"))?;
    let allowed = linux::affinity()?;
    let only = topology.only(&allowed);
    if only.cpus.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            // alloc: the error's message
            format!("the process's affinity mask {allowed:?} has no online CPU"),
        ));
    }
    Ok(only)
}

#[cfg(target_os = "macos")]
fn detect() -> io::Result<Topology> {
    let say = |name: &str| {
        macos::sysctl(name).ok_or_else(|| {
            // alloc: the error's message
            io::Error::new(io::ErrorKind::Unsupported, format!("sysctl {name} says nothing"))
        })
    };
    let levels = say("hw.nperflevels")?;
    // alloc: once per pool, setup
    let mut shape = Vec::new();
    for level in 0..levels {
        // alloc: a sysctl name, once per performance level per pool
        let logical = say(&format!("hw.perflevel{level}.logicalcpu"))?;
        // alloc: a sysctl name, once per performance level per pool
        let per_l2 = say(&format!("hw.perflevel{level}.cpusperl2"))?;
        shape.push((logical, per_l2));
    }
    let topology = Topology::from_perflevels(&shape);
    if topology.cpus.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "hw.perflevel* say no CPU"));
    }
    Ok(topology)
}

/// Where the OS says no more than a count, the pool's threads are not bound ([`crate::pool`]).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn detect() -> io::Result<Topology> {
    std::thread::available_parallelism().map(|it| Topology::flat(it.get()))
}

/// The CPU's NUMA node: its `nodeN` entry, 0 where it has none (a machine of one node).
fn node_of(cpu: &Path) -> io::Result<u32> {
    let entries = match fs::read_dir(cpu) {
        Ok(entries) => entries,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(why) => return Err(at(cpu, &why)),
    };
    for entry in entries {
        let entry = entry.map_err(|why| at(cpu, &why))?;
        let name = entry.file_name();
        if let Some(number) = name.to_str().and_then(|it| it.strip_prefix("node"))
            && let Ok(node) = number.parse::<u32>()
        {
            return Ok(node);
        }
    }
    Ok(0)
}

// BTreeSet for CPU lists: intersections (`&`) and iteration in CPU order, which a group's key needs.

/// The level 3 cache's CPUs, where the CPU says.
fn last_level(cpu: &Path) -> io::Result<Option<BTreeSet<u32>>> {
    let dir = cpu.join("cache");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(why) => return Err(at(&dir, &why)),
    };
    // alloc: once per CPU per pool, setup
    let mut indexes: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|why| at(&dir, &why))?;
        if entry.file_name().to_string_lossy().starts_with("index") {
            indexes.push(entry.path());
        }
    }
    indexes.sort();
    for index in indexes {
        if read_opt(&index.join("level"))?.is_some_and(|it| it.trim() == "3")
            && let Some(text) = read_opt(&index.join("shared_cpu_list"))?
        {
            let path = index.join("shared_cpu_list");
            return list(&text).map(Some).map_err(|why| at(&path, &why));
        }
    }
    Ok(None)
}

/// The CPU's cluster, where it is more than the CPU's core.
fn cluster(cpu: &Path, id: u32, online: &BTreeSet<u32>) -> io::Result<Option<BTreeSet<u32>>> {
    let path = cpu.join("topology/cluster_cpus_list");
    let Some(text) = read_opt(&path)? else { return Ok(None) };
    let cluster = &list(&text).map_err(|why| at(&path, &why))? & online;
    let mut core = BTreeSet::from([id]);
    for name in ["topology/core_cpus_list", "topology/thread_siblings_list"] {
        let path = cpu.join(name);
        if let Some(text) = read_opt(&path)? {
            core.extend(&list(&text).map_err(|why| at(&path, &why))? & online);
            break;
        }
    }
    Ok((cluster.len() > core.len()).then_some(cluster))
}

/// A kernel CPU list: `0-3,8,10-11`.
fn list(text: &str) -> io::Result<BTreeSet<u32>> {
    let number = |it: &str| {
        it.trim().parse::<u32>().map_err(|_| {
            // alloc: the error's message
            io::Error::new(io::ErrorKind::InvalidData, format!("{it:?} is not a CPU number"))
        })
    };
    let mut cpus = BTreeSet::new();
    for part in text.trim().split(',').filter(|it| !it.trim().is_empty()) {
        let (from, to) = match part.split_once('-') {
            Some((from, to)) => (number(from)?, number(to)?),
            None => (number(part)?, number(part)?),
        };
        if from > to {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                // alloc: the error's message
                format!("{part:?} is a range that ends before it begins"),
            ));
        }
        cpus.extend(from..=to);
    }
    Ok(cpus)
}

fn read(path: &Path) -> io::Result<String> {
    fs::read_to_string(path).map_err(|why| at(path, &why))
}

fn read_opt(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(why) => Err(at(path, &why)),
    }
}

fn at(path: &Path, why: &io::Error) -> io::Error {
    // alloc: the error's message
    io::Error::new(why.kind(), format!("{}: {why}", path.display()))
}

/// The affinity calls: the kernel reads or writes a CPU mask through a pointer this passes,
/// which only `libc`'s raw functions reach.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) mod linux {
    use std::{collections::BTreeSet, io, mem::size_of};

    const BITS: usize = 8 * size_of::<libc::c_ulong>();

    /// The CPUs the process may run on.
    pub(crate) fn affinity() -> io::Result<BTreeSet<u32>> {
        let mut words = 1024 / BITS;
        loop {
            // alloc: the affinity mask, once per pool
            let mut mask: Vec<libc::c_ulong> = vec![0; words];
            // SAFETY: the kernel writes at most `words * size_of::<c_ulong>()` bytes of the mask,
            // which is what `mask` holds; a `cpu_set_t` is such words.
            let got = unsafe {
                libc::sched_getaffinity(
                    0,
                    words * size_of::<libc::c_ulong>(),
                    mask.as_mut_ptr().cast::<libc::cpu_set_t>(),
                )
            };
            if got == 0 {
                let mut cpus = BTreeSet::new();
                for (at, word) in mask.iter().enumerate() {
                    for bit in 0..BITS {
                        if word & (1 << bit) != 0 {
                            // `words` is at most 2^16, so the CPU number fits.
                            cpus.insert((at * BITS + bit) as u32);
                        }
                    }
                }
                return Ok(cpus);
            }
            let why = io::Error::last_os_error();
            // EINVAL: the mask is smaller than the kernel's.
            if why.raw_os_error() == Some(libc::EINVAL) && words < 1 << 16 {
                words *= 2;
                continue;
            }
            // alloc: the error's message
            return Err(io::Error::new(why.kind(), format!("sched_getaffinity: {why}")));
        }
    }

    /// Binds the calling thread to `cpu`.
    pub(crate) fn bind(cpu: u32) -> io::Result<()> {
        let cpu = cpu as usize;
        // alloc: the affinity mask, once per worker start
        let mut mask: Vec<libc::c_ulong> = vec![0; (cpu / BITS + 1).max(1024 / BITS)];
        mask[cpu / BITS] |= 1 << (cpu % BITS);
        // SAFETY: the kernel reads `mask.len() * size_of::<c_ulong>()` bytes, which `mask` holds.
        let set = unsafe {
            libc::sched_setaffinity(
                0,
                mask.len() * size_of::<libc::c_ulong>(),
                mask.as_ptr().cast::<libc::cpu_set_t>(),
            )
        };
        if set == 0 {
            Ok(())
        } else {
            let why = io::Error::last_os_error();
            // alloc: the error's message
            Err(io::Error::new(why.kind(), format!("binding a worker to CPU {cpu}: {why}")))
        }
    }
}

/// `sysctlbyname`, which writes into a buffer this passes.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub(crate) mod macos {
    use std::{ffi::CString, mem::size_of, ptr};

    /// A 32-bit integer the kernel says by `name`, if it says it.
    pub(crate) fn sysctl(name: &str) -> Option<u32> {
        let name = CString::new(name).ok()?;
        let mut value: i32 = 0;
        let mut len = size_of::<i32>();
        // SAFETY: `value` has room for the `len` bytes the kernel may write, and `name` is a C
        // string that outlives the call.
        let got = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&raw mut value).cast::<libc::c_void>(),
                &mut len,
                ptr::null_mut(),
                0,
            )
        };
        (got == 0 && len == size_of::<i32>()).then(|| u32::try_from(value).ok()).flatten()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::PathBuf};

    use super::{Cpu, Topology};

    /// A fake `/sys/devices/system/cpu`, removed when dropped.
    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Tree {
            let dir = std::env::temp_dir()
                .join(format!("blaze-topology-{}-{name}", std::process::id()));
            _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Tree(dir)
        }

        fn file(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, format!("{text}\n")).unwrap();
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            _ = fs::remove_dir_all(&self.0);
        }
    }

    fn groups(topology: &Topology) -> Vec<u32> {
        topology.cpus.iter().map(|it| it.group).collect()
    }

    /// Two sockets of four CCDs of eight cores, each core two threads (`c` and `c + 64`). The
    /// SMT siblings say they are a cluster, which is one core: it says nothing.
    #[test]
    fn an_epyc_groups_by_ccd_and_steals_from_its_socket_first() {
        let tree = Tree::new("epyc");
        tree.file("online", "0-127");
        for cpu in 0..128u32 {
            let core = cpu % 64;
            let ccd = core / 8;
            let siblings = format!("{core},{}", core + 64);
            let at = format!("cpu{cpu}");
            tree.file(&format!("{at}/cache/index0/level"), "1");
            tree.file(&format!("{at}/cache/index0/shared_cpu_list"), &siblings);
            tree.file(&format!("{at}/cache/index2/level"), "2");
            tree.file(&format!("{at}/cache/index2/shared_cpu_list"), &siblings);
            tree.file(&format!("{at}/cache/index3/level"), "3");
            tree.file(
                &format!("{at}/cache/index3/shared_cpu_list"),
                &format!("{}-{},{}-{}", ccd * 8, ccd * 8 + 7, 64 + ccd * 8, 64 + ccd * 8 + 7),
            );
            tree.file(&format!("{at}/topology/physical_package_id"), &(ccd / 4).to_string());
            tree.file(&format!("{at}/topology/cluster_cpus_list"), &siblings);
            tree.file(&format!("{at}/topology/core_cpus_list"), &siblings);
        }
        let topology = Topology::from_sysfs(&tree.0).unwrap();
        assert_eq!(topology.cpus.len(), 128);
        assert_eq!(topology.groups(), 8);
        for cpu in &topology.cpus {
            let ccd = (cpu.id % 64) / 8;
            assert_eq!((cpu.group, cpu.socket, cpu.fast), (ccd, ccd / 4, true), "{cpu:?}");
        }
        assert_eq!(topology.steal_order(0), [0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(topology.steal_order(5), [5, 4, 6, 7, 3, 2, 1, 0]);
        assert_eq!(topology.steal_order(8), Vec::<u32>::new());
    }

    /// One L3 for the whole chip, clusters of four cores: the cluster is smaller, so it wins.
    #[test]
    fn a_graviton_groups_by_cluster() {
        let tree = Tree::new("graviton");
        tree.file("online", "0-63");
        for cpu in 0..64u32 {
            let at = format!("cpu{cpu}");
            let first = cpu / 4 * 4;
            tree.file(&format!("{at}/cache/index3/level"), "3");
            tree.file(&format!("{at}/cache/index3/shared_cpu_list"), "0-63");
            tree.file(&format!("{at}/topology/physical_package_id"), "0");
            tree.file(
                &format!("{at}/topology/cluster_cpus_list"),
                &format!("{first}-{}", first + 3),
            );
            tree.file(&format!("{at}/topology/core_cpus_list"), &cpu.to_string());
        }
        let topology = Topology::from_sysfs(&tree.0).unwrap();
        assert_eq!(topology.groups(), 16);
        assert!(topology.cpus.iter().all(|it| it.group == it.id / 4 && it.socket == 0));
        assert_eq!(topology.steal_order(3)[..7], [3, 2, 4, 1, 5, 0, 6]);
    }

    #[test]
    fn a_machine_that_says_nothing_is_one_group() {
        let tree = Tree::new("silent");
        tree.file("online", "0-3");
        let topology = Topology::from_sysfs(&tree.0).unwrap();
        assert_eq!(topology, Topology::flat(4));
        assert_eq!(topology.steal_order(0), [0]);
    }

    #[test]
    fn offline_cpus_and_the_affinity_mask_are_left_out() {
        let tree = Tree::new("offline");
        tree.file("online", "0-5,7");
        for cpu in 0..8u32 {
            let at = format!("cpu{cpu}");
            tree.file(&format!("{at}/cache/index3/level"), "3");
            tree.file(
                &format!("{at}/cache/index3/shared_cpu_list"),
                if cpu < 4 { "0-3" } else { "4-7" },
            );
        }
        let topology = Topology::from_sysfs(&tree.0).unwrap();
        let ids: Vec<u32> = topology.cpus.iter().map(|it| it.id).collect();
        assert_eq!(ids, [0, 1, 2, 3, 4, 5, 7]);
        assert_eq!(groups(&topology), [0, 0, 0, 0, 1, 1, 1]);
        let only = topology.only(&BTreeSet::from([4, 5, 7]));
        assert_eq!(groups(&only), [0, 0, 0]);
        let only = topology.only(&BTreeSet::from([0, 5]));
        assert_eq!(groups(&only), [0, 1]);
        tree.file("online", "0-3,x");
        assert!(Topology::from_sysfs(&tree.0).is_err());
    }

    /// Arm big.LITTLE: the cores of the largest capacity are the fast ones.
    #[test]
    fn the_largest_capacity_is_fast() {
        let tree = Tree::new("biglittle");
        tree.file("online", "0-7");
        for cpu in 0..8u32 {
            let at = format!("cpu{cpu}");
            tree.file(&format!("{at}/cpu_capacity"), if cpu < 4 { "446" } else { "1024" });
            tree.file(
                &format!("{at}/topology/cluster_cpus_list"),
                if cpu < 4 { "0-3" } else { "4-7" },
            );
        }
        let topology = Topology::from_sysfs(&tree.0).unwrap();
        let fast: Vec<bool> = topology.cpus.iter().map(|it| it.fast).collect();
        assert_eq!(fast, [false, false, false, false, true, true, true, true]);
        assert_eq!(groups(&topology), [0, 0, 0, 0, 1, 1, 1, 1]);
    }

    /// An M-series Max: twelve performance cores in L2 clusters of six, four efficiency cores
    /// sharing one.
    #[test]
    fn apple_groups_by_l2_cluster_and_level_zero_is_fast() {
        let topology = Topology::from_perflevels(&[(12, 6), (4, 4)]);
        assert_eq!(topology.cpus.len(), 16);
        assert_eq!(groups(&topology), [0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2]);
        assert!(topology.cpus.iter().all(|it| it.fast == (it.id < 12)));
        assert_eq!(topology.steal_order(2), [2, 1, 0]);
        assert_eq!(
            Topology::from_perflevels(&[(2, 0)]).cpus,
            [
                Cpu { id: 0, group: 0, socket: 0, node: 0, fast: true },
                Cpu { id: 1, group: 0, socket: 0, node: 0, fast: true }
            ]
        );
    }

    /// One socket of two NUMA nodes of four CPUs, one L3 for all eight (sub-NUMA clusters):
    /// each node's part of the cache is a group, and a group steals from its node first.
    #[test]
    fn a_group_never_spans_numa_nodes() {
        let tree = Tree::new("numa");
        tree.file("online", "0-7");
        for cpu in 0..8u32 {
            let at = format!("cpu{cpu}");
            tree.file(&format!("{at}/cache/index3/level"), "3");
            tree.file(&format!("{at}/cache/index3/shared_cpu_list"), "0-7");
            tree.file(&format!("{at}/topology/physical_package_id"), "0");
            // The kernel's is a symlink to the node's directory; only the name is read.
            fs::create_dir_all(tree.0.join(format!("{at}/node{}", cpu / 4))).unwrap();
        }
        let topology = Topology::from_sysfs(&tree.0).unwrap();
        let nodes: Vec<u32> = topology.cpus.iter().map(|it| it.node).collect();
        assert_eq!(nodes, [0, 0, 0, 0, 1, 1, 1, 1]);
        assert_eq!(groups(&topology), [0, 0, 0, 0, 1, 1, 1, 1]);
        assert_eq!(topology.nodes(), [0, 1]);

        // Two sockets of two nodes, a CCD (L3) of two CPUs each: node, then socket, then any.
        let tree = Tree::new("numa-sockets");
        tree.file("online", "0-15");
        for cpu in 0..16u32 {
            let at = format!("cpu{cpu}");
            let ccd = cpu / 2;
            tree.file(&format!("{at}/cache/index3/level"), "3");
            tree.file(
                &format!("{at}/cache/index3/shared_cpu_list"),
                &format!("{}-{}", ccd * 2, ccd * 2 + 1),
            );
            tree.file(&format!("{at}/topology/physical_package_id"), &(cpu / 8).to_string());
            fs::create_dir_all(tree.0.join(format!("{at}/node{}", cpu / 4))).unwrap();
        }
        let topology = Topology::from_sysfs(&tree.0).unwrap();
        assert_eq!(topology.groups(), 8);
        assert_eq!(topology.nodes(), [0, 0, 1, 1, 2, 2, 3, 3]);
        assert_eq!(topology.steal_order(2), [2, 3, 1, 0, 4, 5, 6, 7]);
        assert_eq!(topology.steal_order(5), [5, 4, 6, 7, 3, 2, 1, 0]);
    }

    /// Groups given out of order and spread over sockets and nodes: the steal order is the same
    /// whatever order the CPUs are listed in.
    #[test]
    fn the_steal_order_does_not_depend_on_the_order_cpus_are_listed() {
        let cpu = |id, group, socket, node| Cpu { id, group, socket, node, fast: true };
        let mut cpus: Vec<Cpu> =
            (0..32u32).map(|id| cpu(id, (id * 7) % 16, (id * 7) % 16 / 8, (id * 7) % 16 / 4)).collect();
        let forward = Topology { cpus: cpus.clone() };
        cpus.reverse();
        let backward = Topology { cpus };
        for group in 0..16 {
            let order = forward.steal_order(group);
            assert_eq!(order, backward.steal_order(group));
            let mut expected: Vec<u32> = (0..16).filter(|&it| it != group).collect();
            expected.sort_by_key(|&it| {
                (it / 8 != group / 8, it / 4 != group / 4, it.abs_diff(group), it)
            });
            expected.insert(0, group);
            assert_eq!(order, expected);
        }
    }

    #[test]
    fn detect_finds_at_least_one_cpu() {
        let topology = Topology::detect().unwrap();
        assert!(!topology.cpus.is_empty());
        let groups = topology.groups();
        assert!(topology.cpus.iter().all(|it| it.group < groups));
    }
}
