//! Zero-copy Git binary `commit-graph` (`CGPH` v1) parser.
//!
//! Reads `.git/objects/info/commit-graph` (and split `commit-graph-chain` layers) directly from
//! borrowed/mapped bytes with zero heap allocation per commit. Provides O(log N) lookup by OID
//! via the 256-entry fanout table (`OIDF` + `OIDL`), returning root tree OID, parent indices/OIDs
//! (`CDAT` + `EDGE`), and topological generation numbers (`generation >= 1`) so ahead/behind
//! divergence walks avoid decompressing a single commit object from packfiles.

#![blaze::sans_io]

use blaze_git_fetch::Oid;
use blaze_own::{AVec, Alloc, Vec};

use crate::{ErrorKind, GitError, GitFile};

const PARENT_NONE: u32 = 0x7000_0000;
const EDGE_MASK: u32 = 0x8000_0000;

/// One parsed `CGPH` v1 commit-graph layer, zero-copy over its mapped bytes.
#[derive(Clone, Copy)]
pub struct CommitGraphLayer<'b> {
    bytes: &'b [u8],
    oidf_off: usize,
    oidl_off: usize,
    cdat_off: usize,
    edge_off: Option<(usize, usize)>,
    count: u32,
    /// Cumulative commit count of base layers beneath this layer in a split chain.
    base_offset: u32,
}

/// Information for one commit stored in the commit-graph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GraphCommit {
    pub graph_pos: u32,
    pub tree: Oid,
    /// Topological generation number (`0` = not computed in file; `>= 1` valid).
    pub generation: u32,
    pub commit_time: u64,
    parent1: u32,
    parent2: u32,
}

#[inline]
fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

#[inline]
fn be64(bytes: &[u8], at: usize) -> u64 {
    u64::from_be_bytes([
        bytes[at],
        bytes[at + 1],
        bytes[at + 2],
        bytes[at + 3],
        bytes[at + 4],
        bytes[at + 5],
        bytes[at + 6],
        bytes[at + 7],
    ])
}

#[inline]
fn err(at: usize, kind: ErrorKind) -> GitError {
    GitError { file: GitFile::CommitGraph, at: at as u64, kind }
}

impl<'b> CommitGraphLayer<'b> {
    /// Parse a single `CGPH` v1 file. `base_offset` is 0 for a monolithic graph or the sum of
    /// counts of lower layers in a split chain.
    pub fn parse(bytes: &'b [u8], base_offset: u32) -> Result<CommitGraphLayer<'b>, GitError> {
        // 8-byte header + at least 1 sentinel chunk entry (12B) + 20-byte trailer.
        if bytes.len() < 8 + 12 + 20 || &bytes[..4] != b"CGPH" {
            return Err(err(0, ErrorKind::Malformed));
        }
        let version = bytes[4];
        if version != 1 {
            return Err(err(4, ErrorKind::UnsupportedVersion(u32::from(version))));
        }
        let hash_version = bytes[5];
        if hash_version != 1 {
            return Err(err(5, ErrorKind::UnsupportedVersion(u32::from(hash_version))));
        }
        let num_chunks = usize::from(bytes[6]);
        let table_end = 8 + (num_chunks + 1) * 12;
        if table_end + 20 > bytes.len() {
            return Err(err(8, ErrorKind::Malformed));
        }

        let mut oidf: Option<(usize, usize)> = None;
        let mut oidl: Option<(usize, usize)> = None;
        let mut cdat: Option<(usize, usize)> = None;
        let mut edge: Option<(usize, usize)> = None;

        for i in 0..num_chunks {
            let entry = 8 + i * 12;
            let id = &bytes[entry..entry + 4];
            let off = be64(bytes, entry + 4) as usize;
            let next_off = be64(bytes, entry + 12 + 4) as usize;
            if off > next_off || next_off > bytes.len() {
                return Err(err(entry, ErrorKind::Malformed));
            }
            let len = next_off - off;
            match id {
                b"OIDF" => oidf = Some((off, len)),
                b"OIDL" => oidl = Some((off, len)),
                b"CDAT" => cdat = Some((off, len)),
                b"EDGE" => edge = Some((off, len)),
                _ => {}
            }
        }

        let (oidf_off, oidf_len) = oidf.ok_or_else(|| err(8, ErrorKind::Malformed))?;
        let (oidl_off, oidl_len) = oidl.ok_or_else(|| err(8, ErrorKind::Malformed))?;
        let (cdat_off, cdat_len) = cdat.ok_or_else(|| err(8, ErrorKind::Malformed))?;

        if oidf_len != 256 * 4 {
            return Err(err(oidf_off, ErrorKind::Malformed));
        }
        let mut last = 0u32;
        for b in 0..256usize {
            let c = be32(bytes, oidf_off + b * 4);
            if c < last {
                return Err(err(oidf_off + b * 4, ErrorKind::Malformed));
            }
            last = c;
        }
        let count = last;
        if (count as u64) * 20 > oidl_len as u64 || (count as u64) * 36 > cdat_len as u64 {
            return Err(err(cdat_off, ErrorKind::Malformed));
        }
        if let Some((e_off, e_len)) = edge
            && e_len % 4 != 0
        {
            return Err(err(e_off, ErrorKind::Malformed));
        }

        Ok(CommitGraphLayer {
            bytes,
            oidf_off,
            oidl_off,
            cdat_off,
            edge_off: edge,
            count,
            base_offset,
        })
    }

    #[inline]
    pub fn len(&self) -> u32 {
        self.count
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[inline]
    pub fn oid_at_local(&self, local_idx: u32) -> Oid {
        let start = self.oidl_off + (local_idx as usize) * 20;
        Oid(self.bytes[start..start + 20].try_into().expect("20 bytes"))
    }

    /// Binary search for `oid` within this layer via `OIDF` + `OIDL`.
    pub fn find_local(&self, oid: &Oid) -> Option<u32> {
        let first = usize::from(oid.0[0]);
        let mut lo = if first == 0 { 0 } else { be32(self.bytes, self.oidf_off + (first - 1) * 4) };
        let mut hi = be32(self.bytes, self.oidf_off + first * 4);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let start = self.oidl_off + (mid as usize) * 20;
            match self.bytes[start..start + 20].cmp(&oid.0[..]) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// Read commit metadata at `local_idx`.
    pub fn commit_at_local(&self, local_idx: u32) -> GraphCommit {
        let base = self.cdat_off + (local_idx as usize) * 36;
        let tree = Oid(self.bytes[base..base + 20].try_into().expect("20 bytes"));
        let parent1 = be32(self.bytes, base + 20);
        let parent2 = be32(self.bytes, base + 24);
        let gen_date = be64(self.bytes, base + 28);
        let generation = (gen_date >> 34) as u32;
        let commit_time = gen_date & 0x0000_0003_ffff_ffff;
        GraphCommit {
            graph_pos: self.base_offset + local_idx,
            tree,
            generation,
            commit_time,
            parent1,
            parent2,
        }
    }

    /// Append global parent graph positions for `commit` into `out`.
    pub fn parents_of(&self, commit: &GraphCommit, out: &mut AVec<'_, u32>) {
        if commit.parent1 == PARENT_NONE {
            return;
        }
        out.push(commit.parent1);
        if commit.parent2 == PARENT_NONE {
            return;
        }
        if commit.parent2 & EDGE_MASK == 0 {
            out.push(commit.parent2);
        } else if let Some((edge_off, edge_len)) = self.edge_off {
            let mut idx = (commit.parent2 & !EDGE_MASK) as usize;
            let max_entries = edge_len / 4;
            // Cap octopus parents to prevent malformed cycles in EDGE.
            let mut steps = 0usize;
            while idx < max_entries && steps < 64 {
                let raw = be32(self.bytes, edge_off + idx * 4);
                let is_last = raw & EDGE_MASK != 0;
                out.push(raw & !EDGE_MASK);
                if is_last {
                    break;
                }
                idx += 1;
                steps += 1;
            }
        }
    }
}

/// A monolithic or split commit-graph chain.
pub struct CommitGraph<'b, 'a> {
    layers: AVec<'a, CommitGraphLayer<'b>>,
    total: u32,
}

impl<'b, 'a> CommitGraph<'b, 'a> {
    /// Build from one or more ordered layers (oldest base layer first, newest tip layer last).
    pub fn from_layers(raw_layers: &[&'b [u8]], alloc: Alloc<'a>) -> Result<CommitGraph<'b, 'a>, GitError> {
        let mut layers = Vec::with_capacity_in(raw_layers.len(), alloc);
        let mut total = 0u32;
        for &raw in raw_layers {
            let layer = CommitGraphLayer::parse(raw, total)?;
            total = total
                .checked_add(layer.len())
                .ok_or_else(|| err(0, ErrorKind::Malformed))?;
            layers.push(layer);
        }
        Ok(CommitGraph { layers, total })
    }

    #[inline]
    pub fn total_commits(&self) -> u32 {
        self.total
    }

    /// Look up `oid` across layers (newest layer first, where recent commits live).
    pub fn lookup(&self, oid: &Oid) -> Option<GraphCommit> {
        for layer in self.layers.iter().rev() {
            if let Some(local) = layer.find_local(oid) {
                return Some(layer.commit_at_local(local));
            }
        }
        None
    }

    /// Fetch commit by global graph position.
    pub fn get(&self, graph_pos: u32) -> Option<(usize, GraphCommit)> {
        if graph_pos >= self.total {
            return None;
        }
        for (layer_idx, layer) in self.layers.iter().enumerate() {
            if graph_pos >= layer.base_offset && graph_pos < layer.base_offset + layer.len() {
                let local = graph_pos - layer.base_offset;
                return Some((layer_idx, layer.commit_at_local(local)));
            }
        }
        None
    }

    /// Fetch OID by global graph position.
    pub fn oid_of(&self, graph_pos: u32) -> Option<Oid> {
        if graph_pos >= self.total {
            return None;
        }
        for layer in self.layers.iter() {
            if graph_pos >= layer.base_offset && graph_pos < layer.base_offset + layer.len() {
                return Some(layer.oid_at_local(graph_pos - layer.base_offset));
            }
        }
        None
    }

    /// Append parent global graph positions for commit at `(layer_idx, commit)` into `out`.
    pub fn parents_into(&self, layer_idx: usize, commit: &GraphCommit, out: &mut AVec<'_, u32>) {
        if let Some(layer) = self.layers.get(layer_idx) {
            layer.parents_of(commit, out);
        }
    }
}

/// Parse `.git/objects/info/commit-graphs/commit-graph-chain` into ordered layer hashes.
pub fn parse_graph_chain<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Result<AVec<'a, Oid>, GitError> {
    let mut out = Vec::new_in(alloc);
    let text = core::str::from_utf8(bytes).map_err(|_| err(0, ErrorKind::Malformed))?;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let oid = Oid::parse_hex(trimmed.as_bytes()).ok_or_else(|| err(0, ErrorKind::Malformed))?;
        out.push(oid);
    }
    Ok(out)
}
