//! The code map of one link: what each address of an image is, by code identity, so a profile
//! of a run of that build names its functions across builds (docs/v2/plan/design/perf-history.md,
//! "The code map").
//!
//! A link writes one while `<target-dir>/perf` exists, as `maps/<image>-<hash>/<build id>`.
//! A node is a function or static of the build's reach graph, or a folded group's body: its
//! address and size (0 for one with no code of its own, a static), its identity (what it is
//! across builds; none for a static), its `key` (its code with the codegen settings), its
//! `code` (its own code), its name, and the nodes it refers to (the callees its code calls,
//! inlines or builds in, and the statics it reads).
//!
//! The format, `blzpmap1`, all integers little-endian: the magic, the image's path (`u32`
//! length, UTF-8), the node count (`u64`), then each node: address `u64`, size `u64`, a flag
//! byte (bit 0: an identity follows), the identity (32 bytes, when flagged), key and code (32
//! bytes each), the name (`u32` length, UTF-8), and its edges (`u32` count, then each a `u32`
//! node index). A byte the reader does not expect is an error naming where.

use std::path::{Path, PathBuf};

use blaze_ids::{ContentId, Hasher};

use super::{Id, hex};

/// The first bytes of a map: its format and version.
pub const MAGIC: &[u8; 8] = b"blzpmap1";
/// The maps a link keeps of one image: the newest ones. A run recorded keeps `deep` in its
/// rows, so a map collected after only costs the names of the callees that changed.
pub const KEEP: usize = 8;
/// The directory of the maps in `<target-dir>/perf`.
pub const MAPS: &str = "maps";

const DEEP_DOMAIN: &str = "blaze v2 perf deep 1";
const COMPONENT_DOMAIN: &str = "blaze v2 perf component 1";
const IMAGE_DOMAIN: &str = "blaze v2 perf image 1";

/// One node of a map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub address: u64,
    /// 0 for a node with no code of its own.
    pub size: u64,
    pub identity: Option<Id>,
    pub key: Id,
    pub code: Id,
    pub name: String,
    /// The nodes it refers to, as indices into [`Map::nodes`], sorted, each once.
    pub edges: Vec<u32>,
}

/// The code map of one image of one link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Map {
    /// The image's path.
    pub image: String,
    pub nodes: Vec<Node>,
}

impl Map {
    pub fn encode(&self) -> Vec<u8> {
        let names: usize =
            self.nodes.iter().map(|it| it.name.len() + 4 * it.edges.len() + 128).sum();
        // alloc: once per link that keeps a code map
        let mut out = Vec::with_capacity(MAGIC.len() + 12 + self.image.len() + names);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(self.image.len() as u32).to_le_bytes());
        out.extend_from_slice(self.image.as_bytes());
        out.extend_from_slice(&(self.nodes.len() as u64).to_le_bytes());
        for node in &self.nodes {
            out.extend_from_slice(&node.address.to_le_bytes());
            out.extend_from_slice(&node.size.to_le_bytes());
            out.push(u8::from(node.identity.is_some()));
            if let Some(identity) = &node.identity {
                out.extend_from_slice(identity);
            }
            out.extend_from_slice(&node.key);
            out.extend_from_slice(&node.code);
            out.extend_from_slice(&(node.name.len() as u32).to_le_bytes());
            out.extend_from_slice(node.name.as_bytes());
            out.extend_from_slice(&(node.edges.len() as u32).to_le_bytes());
            for edge in &node.edges {
                out.extend_from_slice(&edge.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Map, String> {
        let mut at = Reader { bytes, at: 0 };
        if at.take(MAGIC.len(), "its magic")? != MAGIC {
            // alloc: an error
            return Err(
                "not a code map of this blaze (its first bytes are not `blzpmap1`)".to_owned()
            );
        }
        let len = at.u32("its image's path's length")? as usize;
        let image = at.text(len, "its image's path")?;
        let count = at.u64("its node count")?;
        // Each node takes at least 89 bytes: a count past that is a damaged file, not a reason
        // to reserve memory.
        if count > (bytes.len() / 89) as u64 {
            // alloc: an error
            return Err(format!("it says {count} nodes, which {} bytes can't hold", bytes.len()));
        }
        // alloc: once per map read, its nodes
        let mut nodes = Vec::with_capacity(count as usize);
        for index in 0..count {
            let address = at.u64("a node's address")?;
            let size = at.u64("a node's size")?;
            let identity = match at.take(1, "a node's flags")?[0] {
                0 => None,
                1 => Some(at.id("a node's identity")?),
                // alloc: an error
                other => {
                    return Err(format!(
                        "node {index} has flags {other:#x}, which no blaze writes"
                    ));
                }
            };
            let key = at.id("a node's key")?;
            let code = at.id("a node's code")?;
            let len = at.u32("a node's name's length")? as usize;
            let name = at.text(len, "a node's name")?;
            let edge_count = at.u32("a node's edge count")?;
            if edge_count as usize > at.left() / 4 {
                // alloc: an error
                return Err(format!("node {index} says {edge_count} edges, past the file's end"));
            }
            // alloc: once per map read, a node's edges
            let mut edges = Vec::with_capacity(edge_count as usize);
            for _ in 0..edge_count {
                let edge = at.u32("an edge")?;
                if u64::from(edge) >= count {
                    // alloc: an error
                    return Err(format!(
                        "node {index} refers to node {edge}, past its {count} nodes"
                    ));
                }
                edges.push(edge);
            }
            nodes.push(Node { address, size, identity, key, code, name, edges });
        }
        if at.left() != 0 {
            // alloc: an error
            return Err(format!("{} bytes follow its last node", at.left()));
        }
        Ok(Map { image, nodes })
    }

    /// The nodes with code, by address: what [`Map::function_at`] searches.
    pub fn by_address(&self) -> Vec<u32> {
        // alloc: once per map read
        let mut out: Vec<u32> =
            (0..self.nodes.len() as u32).filter(|it| self.nodes[*it as usize].size > 0).collect();
        out.sort_unstable_by_key(|it| (self.nodes[*it as usize].address, *it));
        out
    }

    /// The node whose code holds `address`, through `by_address` ([`Map::by_address`]).
    pub fn function_at(&self, by_address: &[u32], address: u64) -> Option<u32> {
        let at = by_address
            .partition_point(|it| self.nodes[*it as usize].address <= address)
            .checked_sub(1)?;
        let node = by_address[at];
        let it = &self.nodes[node as usize];
        (address - it.address < it.size).then_some(node)
    }

    /// Each node's `deep`: a hash of its key and of the `deep` of everything it refers to,
    /// transitively. A strongly connected component (recursion) is hashed as one: its members'
    /// keys, sorted, and the `deep` of every node outside it that a member refers to, sorted,
    /// each once; a member's `deep` is that hash with its own key. A node on no cycle is a
    /// component of one, by the same formula. Tarjan's walk, iterative and bounded by the
    /// graph's size.
    pub fn deep(&self) -> Result<Vec<Id>, String> {
        let n = self.nodes.len();
        const UNSEEN: u32 = u32::MAX;
        // alloc: once per map, per node
        let mut index = vec![UNSEEN; n];
        // alloc: once per map, per node
        let mut low = vec![0u32; n];
        // alloc: once per map, per node
        let mut on_stack = vec![false; n];
        // alloc: once per map, per node
        let mut deep: Vec<Option<Id>> = vec![None; n];
        // alloc: once per map
        let mut stack: Vec<u32> = Vec::new();
        // alloc: once per map
        let mut calls: Vec<(u32, usize)> = Vec::new();
        let mut next = 0u32;
        // Every node is entered once and every edge followed once, and each component leaves
        // once: past that the walk is not walking this graph.
        let edges: usize = self.nodes.iter().map(|it| it.edges.len()).sum();
        let mut fuel = 2 * n + edges + 1;
        for root in 0..n as u32 {
            if index[root as usize] != UNSEEN {
                continue;
            }
            calls.push((root, 0));
            index[root as usize] = next;
            low[root as usize] = next;
            next += 1;
            stack.push(root);
            on_stack[root as usize] = true;
            while let Some(&mut (node, ref mut edge)) = calls.last_mut() {
                fuel = fuel.checked_sub(1).ok_or_else(|| {
                    // alloc: an error
                    format!(
                        "the deep hash of {}'s code map ran past its {n} nodes and {edges} edges",
                        self.image
                    )
                })?;
                let edges = &self.nodes[node as usize].edges;
                if let Some(&callee) = edges.get(*edge) {
                    *edge += 1;
                    let c = callee as usize;
                    if index[c] == UNSEEN {
                        index[c] = next;
                        low[c] = next;
                        next += 1;
                        stack.push(callee);
                        on_stack[c] = true;
                        calls.push((callee, 0));
                    } else if on_stack[c] {
                        low[node as usize] = low[node as usize].min(index[c]);
                    }
                    continue;
                }
                calls.pop();
                if let Some(&(caller, _)) = calls.last() {
                    low[caller as usize] = low[caller as usize].min(low[node as usize]);
                }
                if low[node as usize] != index[node as usize] {
                    continue;
                }
                // `node` roots a component: its members are the stack down to it.
                let from = stack.iter().rposition(|it| *it == node).unwrap_or(0);
                // alloc: once per component
                let members: Vec<u32> = stack.drain(from..).collect();
                for it in &members {
                    on_stack[*it as usize] = false;
                }
                // alloc: once per component
                let mut keys: Vec<Id> =
                    members.iter().map(|it| self.nodes[*it as usize].key).collect();
                keys.sort_unstable();
                // alloc: once per component
                let mut outside: Vec<Id> = Vec::new();
                for member in &members {
                    for &callee in &self.nodes[*member as usize].edges {
                        if members.contains(&callee) {
                            continue;
                        }
                        // Tarjan closes a component after every one it reaches.
                        let Some(it) = deep[callee as usize] else {
                            // alloc: an error
                            return Err(format!(
                                "the deep hash of {}'s code map reached node {callee} before its component closed",
                                self.image
                            ));
                        };
                        outside.push(it);
                    }
                }
                outside.sort_unstable();
                outside.dedup();
                let mut hasher = Hasher::new(COMPONENT_DOMAIN);
                hasher.update(&(keys.len() as u64).to_le_bytes());
                for key in &keys {
                    hasher.update(key);
                }
                hasher.update(&(outside.len() as u64).to_le_bytes());
                for it in &outside {
                    hasher.update(it);
                }
                let component = hasher.finish();
                for member in &members {
                    let mut hasher = Hasher::new(DEEP_DOMAIN);
                    hasher.id(&component).update(&self.nodes[*member as usize].key);
                    deep[*member as usize] = Some(hasher.finish().0);
                }
            }
        }
        deep.into_iter()
            .enumerate()
            // alloc: an error
            .map(|(at, it)| {
                it.ok_or_else(|| format!("node {at} of {}'s code map was never hashed", self.image))
            })
            .collect()
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn left(&self) -> usize {
        self.bytes.len() - self.at
    }

    fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(len).filter(|it| *it <= self.bytes.len());
        // alloc: an error
        let end = end.ok_or_else(|| format!("cut short at byte {} in {what}", self.at))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u32(&mut self, what: &str) -> Result<u32, String> {
        let bytes = self.take(4, what)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self, what: &str) -> Result<u64, String> {
        let mut out = [0; 8];
        out.copy_from_slice(self.take(8, what)?);
        Ok(u64::from_le_bytes(out))
    }

    fn id(&mut self, what: &str) -> Result<Id, String> {
        let mut out = [0; 32];
        out.copy_from_slice(self.take(32, what)?);
        Ok(out)
    }

    fn text(&mut self, len: usize, what: &str) -> Result<String, String> {
        let at = self.at;
        let bytes = self.take(len, what)?;
        // alloc: once per name read
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| format!("{what} at byte {at} is not UTF-8"))
    }
}

/// The directory of the maps of the image at `image`, in `perf` (`<target-dir>/perf`): its
/// file name, then a hash of the whole path, so two images of one name keep two.
pub fn image_dir(perf: &Path, image: &str) -> PathBuf {
    let name = image.rsplit('/').next().unwrap_or(image);
    let id = ContentId::of(IMAGE_DOMAIN, image.as_bytes());
    // alloc: a path, once per link that keeps a map
    perf.join(MAPS).join(format!("{name}-{}", hex(&id.0[..8])))
}

/// Writes `map`, of the link whose build id is `build_id` (hex), into `perf`
/// (`<target-dir>/perf`), whole or not at all, then removes that image's maps but the newest
/// [`KEEP`]. One build id is one map: a different map already there under it is refused, never
/// written over.
pub fn write(perf: &Path, build_id: &str, map: &Map) -> Result<(), String> {
    let dir = image_dir(perf, &map.image);
    std::fs::create_dir_all(&dir).map_err(|error| format!("making {}: {error}", dir.display()))?;
    let bytes = map.encode();
    let file = dir.join(build_id);
    match std::fs::read(&file) {
        Ok(kept) if kept == bytes => return collect(&dir),
        Ok(_) => {
            return Err(format!(
                "the code map {} names other code for this link's build id {build_id}: two links of {} gave one build \
                 id to different code; remove {} to profile it afresh",
                file.display(),
                map.image,
                dir.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", file.display())),
    }
    let part = dir.join(format!(".{build_id}.part-{}", std::process::id()));
    std::fs::write(&part, &bytes)
        .map_err(|error| format!("writing {}: {error}", part.display()))?;
    std::fs::rename(&part, &file)
        .map_err(|error| format!("renaming {} to {}: {error}", part.display(), file.display()))?;
    collect(&dir)
}

/// Removes the maps in `dir` (one image's) but the newest [`KEEP`]; ties by name.
fn collect(dir: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    // alloc: once per link that keeps a map, a map of its image's
    let mut maps: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
        if entry.file_name().to_str().is_none_or(|it| it.starts_with('.')) {
            continue;
        }
        let modified = match entry.metadata().and_then(|it| it.modified()) {
            Ok(modified) => modified,
            // Gone: another link's collection of this image took it first.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{}: {error}", entry.path().display())),
        };
        maps.push((modified, entry.path()));
    }
    maps.sort_unstable_by(|a, b| b.cmp(a));
    for (_, at) in maps.iter().skip(KEEP) {
        match std::fs::remove_file(at) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("removing {}: {error}", at.display())),
        }
    }
    Ok(())
}

/// The map of the link whose build id is `build_id` (hex), from any image's directory in
/// `perf`, with its path; none when no map of it is kept.
pub fn read(perf: &Path, build_id: &str) -> Result<Option<(PathBuf, Map)>, String> {
    let root = perf.join(MAPS);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", root.display())),
    };
    // A build id is hex digits: anything else would name a path outside the maps.
    if build_id.is_empty() || !build_id.bytes().all(|it| it.is_ascii_hexdigit()) {
        return Ok(None);
    }
    // alloc: once per build id read, the images kept
    let mut images = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", root.display()))?;
        if entry
            .file_type()
            .map_err(|error| format!("{}: {error}", entry.path().display()))?
            .is_dir()
        {
            images.push(entry.path());
        }
    }
    images.sort_unstable();
    for image in images {
        let file = image.join(build_id);
        match std::fs::read(&file) {
            Ok(bytes) => {
                let map = Map::decode(&bytes)
                    .map_err(|why| format!("{}: {why}; remove it", file.display()))?;
                return Ok(Some((file, map)));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{}: {error}", file.display())),
        }
    }
    Ok(None)
}
