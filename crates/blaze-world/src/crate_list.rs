//! The crate list as a record (D114 rule 1, the coordinator's ruling 2026-11-16 21:1x): what a
//! reader of the world needs of each crate, written ONCE by the job that makes the world (the
//! `UnitGraph` task) and read IN PLACE by a search, so a body edit makes no world: no shelf
//! opened, no package placed, no list made.
//!
//! One row a crate, sorted by crate key (strictly: two crates under one key are refused when
//! written, rule 21). Positions are tree paths (`/<tree>/...`, the world's), never a path of
//! this machine (rules 25, 71). Nothing of a crate's TEXT is here: a fact of a root file is read
//! from that file's outline record by whoever needs it.
//!
//! THE TREES. A row's paths are `/<tree>/..`; tree 0 is the checkout and tree `k` (1 and up)
//! is the world's `k`th pack ([`crate::Texts::packs`]). The tree table names each pack by
//! content, as the world knew it when the list was made ([`crate::PackOrigin`]: a toolchain
//! pack by its shelf digest, a fetched package's record by its store key), with the BLAKE3 of
//! its files' listing (`PackView::tree`), which its opener checks: so a reader with no world (a
//! body job, blaze-cli body_types.rs) opens the very packs the world read, in place, and serves
//! every dependency's file text. The files read corrected (D58) follow: `(tree, path in it,
//! corrected text)`, read in place of the pack's bytes as the world reads them.
//!
//! Form (little endian), checked whole once at [`CrateList::open`], then read without checks:
//! a 44-byte header (magic, version, rows `n`, deps `d`, heap length, sysroot core, alloc, std,
//! bstd as row indices or `NONE`, trees `t`, corrected files `c`), `n` rows of [`ROW`] bytes,
//! `d` dependency entries of [`DEP`] bytes, `t` trees of [`TREE`] bytes, `c` corrected files of
//! [`CORRECTED`] bytes, then the string heap.

use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_scope::crate_env::{EnvFacts, SeenRoot};
use blaze_scope::resolve::SourceTexts;

use crate::{Origin, PackOrigin, Side, World};

const MAGIC: &[u8; 4] = b"BZCL";
/// 2: the tree table and the corrected files (a body job serves its dependencies' texts from
/// them); a version-1 list is refused by name at [`CrateList::open`]. 3: each row's
/// environment facts ([`CrateList::env_facts`]: a body's `env!` is answered from them).
pub const VERSION: u16 = 3;
/// magic 4, version 2, pad 2, `n` 4, `d` 4, heap 4, sysroot 16, `t` 4, `c` 4.
pub const HEADER_LEN: usize = 44;
/// kind 1 ([`SHELF`] or [`SOURCE`]), pad 3, its name 32 (shelf digest or store key), its files'
/// listing BLAKE3 32.
pub const TREE: usize = 68;
/// tree 4, path 8, text 8.
pub const CORRECTED: usize = 20;
/// A tree whose pack is on the toolchain's shelf ([`PackOrigin::Shelf`]).
const SHELF: u8 = 1;
/// A tree whose pack is a record of the sources store ([`PackOrigin::Source`]).
const SOURCE: u8 = 2;
/// key 32, name 8, root 8, shown_from 8, deps 8, side 1, flags 1, script kind 1, pad 1, script
/// subject 32, then the crate's environment facts (blaze-scope `EnvFacts`, from its
/// `CrateEnv::facts`): crate name 8, package name 8, package version 8, binary name 8, the
/// member's dir 8, env flags 1 ([`ENV_PACKAGE`] ..), pad 3.
pub const ROW: usize = 144;
/// Where a row's environment facts start.
const ENV_AT: usize = 100;

/// The crate has a package (name and version at 108, 116); a toolchain crate has none.
const ENV_PACKAGE: u8 = 1;
/// A binary: its name at 124.
const ENV_BIN: u8 = 2;
/// Its package has a build script.
const ENV_BUILD_SCRIPT: u8 = 4;
/// An integration test or a bench.
const ENV_TEST_OR_BENCH: u8 = 8;
/// It sees its workspace member's dir (at 132; D96 `SeenRoot::Member`), else `/package`.
const ENV_MEMBER: u8 = 16;
const ENV_FLAGS: u8 = ENV_PACKAGE | ENV_BIN | ENV_BUILD_SCRIPT | ENV_TEST_OR_BENCH | ENV_MEMBER;
/// name 8, row 4.
pub const DEP: usize = 12;
/// No row (a sysroot crate the world has not).
pub const NONE: u32 = u32::MAX;

const PROC_MACRO: u8 = 1;
const TOOLCHAIN: u8 = 2;
const MEMBER: u8 = 4;
const REFUSED: u8 = 8;

/// What a reader of a crate graph asks: the world made fresh, or the list read in place. The
/// check scope's rule ([`crate::check_scope::check_scope_in`]) is written once over it (rule 78).
/// `'n`: what its names borrow (the world's, or the list's bytes).
pub trait CrateGraph<'n> {
    fn len(&self) -> usize;
    fn name(&self, at: usize) -> &'n str;
    fn side(&self, at: usize) -> Side;
    fn proc_macro(&self, at: usize) -> bool;
    fn toolchain(&self, at: usize) -> bool;
    fn refused(&self, at: usize) -> bool;
    /// The crates `at` depends on, by index, in its own order.
    fn deps(&self, at: usize) -> impl Iterator<Item = usize> + '_;
    /// `core`, `alloc`, Rust's `std`, bstd.
    fn sysroot(&self) -> [Option<usize>; 4];
}

impl<'w> CrateGraph<'w> for World<'w> {
    fn len(&self) -> usize {
        self.crates.len()
    }
    fn name(&self, at: usize) -> &'w str {
        self.crates[at].input.name
    }
    fn side(&self, at: usize) -> Side {
        self.crates[at].side
    }
    fn proc_macro(&self, at: usize) -> bool {
        self.crates[at].proc_macro
    }
    fn toolchain(&self, at: usize) -> bool {
        matches!(self.crates[at].origin, Origin::Toolchain(_))
    }
    fn refused(&self, at: usize) -> bool {
        self.refusal(at).is_some()
    }
    fn deps(&self, at: usize) -> impl Iterator<Item = usize> + '_ {
        self.crates[at].input.deps.iter().map(|(_, dep)| *dep)
    }
    fn sysroot(&self) -> [Option<usize>; 4] {
        let s = self.sysroot;
        [s.core, s.alloc, s.std, s.bstd].map(|id| id.map(|id| id.0 as usize))
    }
}

/// Why a list was not written or does not open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListError {
    /// Two crates of the world under one key (rule 21: the key misses an input).
    SameKey { first: usize, second: usize },
    /// A count or a length past what the form holds (`u32`).
    TooLarge { what: &'static str },
    /// The bytes are not a list of this form: what was wrong, and at which byte.
    Form { what: &'static str, at: usize },
    /// A list of another version of the form (one with no tree table is version 1).
    Version { found: u16 },
    /// The world's packs and their origins are not one to one (`Texts::origins`).
    Origins { packs: usize, origins: usize },
    /// The world holds no environment for its crate `at` (`Texts::crate_env`).
    NoEnv { at: usize },
}

impl core::fmt::Display for ListError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ListError::Version { found } => {
                write!(f, "a crate list of version {found}, not {VERSION} (crate_list.rs `VERSION`): written by another blaze, so its unit graph is made again")
            }
            ListError::Origins { packs, origins } => write!(f, "the world has {packs} packs and {origins} pack origins (blaze-world `Texts::origins`)"),
            ListError::SameKey { first, second } => write!(f, "crates {first} and {second} of the world have one key (rule 21: its key misses an input)"),
            ListError::TooLarge { what } => write!(f, "the crate list's {what} is past what its form holds"),
            ListError::Form { what, at } => write!(f, "not a crate list of this form: {what} (at byte {at})"),
            ListError::NoEnv { at } => write!(f, "crate {at} of the world has no environment (blaze-world `Texts::crate_env`)"),
        }
    }
}

fn u32_of(n: usize, what: &'static str) -> Result<u32, ListError> {
    u32::try_from(n).map_err(|_| ListError::TooLarge { what })
}

/// The list of `world`, sorted by key; `script_of(at)` is the name of crate `at`'s `Script` part
/// (its kind byte and 32-byte subject) when it has a build script whose run is a task.
pub fn encode<'a>(world: &World<'_>, script_of: &dyn Fn(usize) -> Option<[u8; 33]>, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, ListError> {
    let crates = &world.crates;
    let n = crates.len();
    // Row order: world indices sorted by key; `row_of[world index]` is the row.
    let mut order: AVec<'_, usize> = AVec::with_capacity_in(n, alloc.clone());
    order.extend(0..n);
    order.sort_unstable_by_key(|at| crates[*at].input.key.0.0);
    if let Some(pair) = order.windows(2).find(|pair| crates[pair[0]].input.key == crates[pair[1]].input.key) {
        return Err(ListError::SameKey { first: pair[0].min(pair[1]), second: pair[0].max(pair[1]) });
    }
    let mut row_of: AVec<'_, u32> = AVec::with_capacity_in(n, alloc.clone());
    row_of.extend((0..n).map(|_| NONE));
    for (row, at) in order.iter().enumerate() {
        row_of[*at] = u32_of(row, "row count")?;
    }
    let mut heap: AVec<'_, u8> = AVec::new_in(alloc.clone());
    let mut rows: AVec<'_, u8> = AVec::with_capacity_in(n * ROW, alloc.clone());
    let mut deps: AVec<'_, u8> = AVec::new_in(alloc.clone());
    let mut d = 0usize;
    for at in order.iter().copied() {
        let c = &crates[at];
        rows.extend_from_slice(&c.input.key.0.0);
        rows.extend_from_slice(&text(c.input.name, &mut heap)?);
        rows.extend_from_slice(&text(c.input.root, &mut heap)?);
        rows.extend_from_slice(&text(c.input.shown_from, &mut heap)?);
        rows.extend_from_slice(&u32_of(d, "dependency count")?.to_le_bytes());
        rows.extend_from_slice(&u32_of(c.input.deps.len(), "dependency count")?.to_le_bytes());
        for (name, dep) in c.input.deps.iter() {
            deps.extend_from_slice(&text(name, &mut heap)?);
            deps.extend_from_slice(&row_of[*dep].to_le_bytes());
            d += 1;
        }
        rows.push(match c.side {
            Side::Target => 0,
            Side::Macro => 1,
        });
        let mut flags = 0;
        if c.proc_macro {
            flags |= PROC_MACRO;
        }
        if matches!(c.origin, Origin::Toolchain(_)) {
            flags |= TOOLCHAIN;
        }
        if c.input.root.starts_with("/0/") {
            flags |= MEMBER;
        }
        if world.refusal(at).is_some() {
            flags |= REFUSED;
        }
        rows.push(flags);
        match script_of(at) {
            Some(name) => {
                // A kind byte of 0 would read as "no script": the name's kind is never 0 here.
                if name[0] == 0 {
                    return Err(ListError::Form { what: "a script part's kind byte is 0", at: rows.len() });
                }
                rows.push(name[0]);
                rows.push(0);
                rows.extend_from_slice(&name[1..]);
            }
            None => {
                rows.extend_from_slice(&[0; 34]);
            }
        }
        // The environment's facts, as the world made it (its `CrateEnv`, never joined with a
        // script's record: that is read by each reader as its own input).
        let env = world.texts.crate_env(at).ok_or(ListError::NoEnv { at })?.facts();
        let mut env_flags = 0;
        rows.extend_from_slice(&text(env.crate_name, &mut heap)?);
        let (package, version) = env.package.unwrap_or(("", ""));
        if env.package.is_some() {
            env_flags |= ENV_PACKAGE;
        }
        rows.extend_from_slice(&text(package, &mut heap)?);
        rows.extend_from_slice(&text(version, &mut heap)?);
        if env.bin.is_some() {
            env_flags |= ENV_BIN;
        }
        rows.extend_from_slice(&text(env.bin.unwrap_or(""), &mut heap)?);
        let dir = match env.seen {
            SeenRoot::Package => "",
            SeenRoot::Member { dir } => {
                env_flags |= ENV_MEMBER;
                dir
            }
        };
        rows.extend_from_slice(&text(dir, &mut heap)?);
        if env.build_script {
            env_flags |= ENV_BUILD_SCRIPT;
        }
        if env.test_or_bench {
            env_flags |= ENV_TEST_OR_BENCH;
        }
        rows.extend_from_slice(&[env_flags, 0, 0, 0]);
    }
    // The trees past the checkout, in the world's order (tree `k` is pack `k - 1`).
    let (packs, origins) = (world.texts.packs(), world.texts.origins());
    if packs.len() != origins.len() {
        return Err(ListError::Origins { packs: packs.len(), origins: origins.len() });
    }
    let mut trees: AVec<'_, u8> = AVec::with_capacity_in(packs.len() * TREE, alloc.clone());
    for (pack, origin) in packs.iter().zip(origins.iter()) {
        let (kind, name) = match origin {
            PackOrigin::Shelf { digest } => (SHELF, *digest),
            PackOrigin::Source { key } => (SOURCE, key.0),
        };
        trees.extend_from_slice(&[kind, 0, 0, 0]);
        trees.extend_from_slice(&name);
        trees.extend_from_slice(&pack.tree().0);
    }
    let corrected_files = world.texts.corrected();
    let mut corrected: AVec<'_, u8> = AVec::with_capacity_in(corrected_files.len() * CORRECTED, alloc.clone());
    for (tree, path, text_of) in corrected_files.iter() {
        corrected.extend_from_slice(&tree.to_le_bytes());
        corrected.extend_from_slice(&text(path, &mut heap)?);
        corrected.extend_from_slice(&text(text_of, &mut heap)?);
    }
    let mut out: AVec<'a, u8> = AVec::with_capacity_in(HEADER_LEN + rows.len() + deps.len() + trees.len() + corrected.len() + heap.len(), alloc);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&u32_of(n, "row count")?.to_le_bytes());
    out.extend_from_slice(&u32_of(d, "dependency count")?.to_le_bytes());
    out.extend_from_slice(&u32_of(heap.len(), "string heap")?.to_le_bytes());
    let s = world.sysroot;
    for id in [s.core, s.alloc, s.std, s.bstd] {
        out.extend_from_slice(&id.map_or(NONE, |id| row_of.get(id.0 as usize).copied().unwrap_or(NONE)).to_le_bytes());
    }
    out.extend_from_slice(&u32_of(packs.len(), "tree count")?.to_le_bytes());
    out.extend_from_slice(&u32_of(corrected_files.len(), "corrected file count")?.to_le_bytes());
    out.extend_from_slice(&rows);
    out.extend_from_slice(&deps);
    out.extend_from_slice(&trees);
    out.extend_from_slice(&corrected);
    out.extend_from_slice(&heap);
    Ok(out)
}

/// Where tree `k` (1 and up) of a list is kept, and its files' identity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TreeRow {
    pub origin: PackOrigin,
    /// The BLAKE3 of the pack's files' listing (`PackView::tree`): its opener checks the pack it
    /// mapped is this one.
    pub files: [u8; 32],
}

/// `s` into `heap`: its (offset, length), each a `u32`.
fn text(s: &str, heap: &mut AVec<'_, u8>) -> Result<[u8; 8], ListError> {
    let off = u32_of(heap.len(), "string heap")?;
    let len = u32_of(s.len(), "string length")?;
    heap.extend_from_slice(s.as_bytes());
    let mut b = [0; 8];
    b[..4].copy_from_slice(&off.to_le_bytes());
    b[4..].copy_from_slice(&len.to_le_bytes());
    Ok(b)
}

/// A list read in place: every range checked once, at [`CrateList::open`].
#[derive(Clone, Copy)]
pub struct CrateList<'b> {
    rows: &'b [u8],
    deps: &'b [u8],
    trees: &'b [u8],
    corrected: &'b [u8],
    heap: &'b str,
    n: usize,
    sysroot: [u32; 4],
}

fn le32(b: &[u8], at: usize) -> u32 {
    let mut w = [0; 4];
    w.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(w)
}

impl<'b> CrateList<'b> {
    /// The list in `bytes`, checked whole: the header, the lengths, every string range in the
    /// heap and on a character boundary, every dependency's row, the rows strictly by key.
    pub fn open(bytes: &'b [u8]) -> Result<CrateList<'b>, ListError> {
        let form = |what: &'static str, at: usize| ListError::Form { what, at };
        if bytes.len() < HEADER_LEN || &bytes[..4] != MAGIC {
            return Err(form("no magic", 0));
        }
        let found = u16::from_le_bytes([bytes[4], bytes[5]]);
        if found != VERSION {
            return Err(ListError::Version { found });
        }
        let n = le32(bytes, 8) as usize;
        let d = le32(bytes, 12) as usize;
        let h = le32(bytes, 16) as usize;
        let t = le32(bytes, 36) as usize;
        let c = le32(bytes, 40) as usize;
        let after = |start: usize, count: usize, size: usize, at: usize| {
            count.checked_mul(size).and_then(|len| start.checked_add(len)).ok_or(form("a table's length overflows", at))
        };
        let rows_end = after(HEADER_LEN, n, ROW, 8)?;
        let deps_end = after(rows_end, d, DEP, 12)?;
        let trees_end = after(deps_end, t, TREE, 36)?;
        let corrected_end = after(trees_end, c, CORRECTED, 40)?;
        if corrected_end.checked_add(h) != Some(bytes.len()) {
            return Err(form("the lengths do not add up to the bytes", 16));
        }
        let heap = core::str::from_utf8(&bytes[corrected_end..]).map_err(|_| form("the string heap is not UTF-8", corrected_end))?;
        let mut sysroot = [NONE; 4];
        for (i, s) in sysroot.iter_mut().enumerate() {
            *s = le32(bytes, 20 + 4 * i);
            if *s != NONE && *s as usize >= n {
                return Err(form("a sysroot row past the rows", 20 + 4 * i));
            }
        }
        let list = CrateList {
            rows: &bytes[HEADER_LEN..rows_end],
            deps: &bytes[rows_end..deps_end],
            trees: &bytes[deps_end..trees_end],
            corrected: &bytes[trees_end..corrected_end],
            heap,
            n,
            sysroot,
        };
        let text_ok = |b: &[u8], at: usize| {
            let (off, len) = (le32(b, at) as usize, le32(b, at + 4) as usize);
            off.checked_add(len).is_some_and(|end| end <= heap.len() && heap.is_char_boundary(off) && heap.is_char_boundary(end))
        };
        for r in 0..n {
            let row = list.row(r);
            for at in [32, 40, 48] {
                if !text_ok(row, at) {
                    return Err(form("a row's string is outside the heap", HEADER_LEN + r * ROW + at));
                }
            }
            let (start, count) = (le32(row, 56) as usize, le32(row, 60) as usize);
            if start.checked_add(count).is_none_or(|end| end > d) {
                return Err(form("a row's dependencies are outside the table", HEADER_LEN + r * ROW + 56));
            }
            if row[64] > 1 {
                return Err(form("a side that is neither target nor macro", HEADER_LEN + r * ROW + 64));
            }
            for at in [ENV_AT, ENV_AT + 8, ENV_AT + 16, ENV_AT + 24, ENV_AT + 32] {
                if !text_ok(row, at) {
                    return Err(form("a row's environment string is outside the heap", HEADER_LEN + r * ROW + at));
                }
            }
            if row[ENV_AT + 40] & !ENV_FLAGS != 0 || row[ENV_AT + 41..ENV_AT + 44] != [0, 0, 0] {
                return Err(form("a row's environment flags name no fact", HEADER_LEN + r * ROW + ENV_AT + 40));
            }
            // The member's dir is checked here once, so the facts read later need no check.
            if row[ENV_AT + 40] & ENV_MEMBER != 0 && SeenRoot::member(list.text(row, ENV_AT + 32)).is_err() {
                return Err(form("a row's member dir is not a plain path under the workspace", HEADER_LEN + r * ROW + ENV_AT + 32));
            }
            if r > 0 && list.row(r - 1)[..32] >= row[..32] {
                return Err(form("the rows are not strictly by key", HEADER_LEN + r * ROW));
            }
        }
        for e in 0..d {
            let dep = &list.deps[e * DEP..(e + 1) * DEP];
            if !text_ok(dep, 0) || le32(dep, 8) as usize >= n {
                return Err(form("a dependency names no row or no string", rows_end + e * DEP));
            }
        }
        for k in 0..t {
            let tree = &list.trees[k * TREE..(k + 1) * TREE];
            if !matches!(tree[0], SHELF | SOURCE) || tree[1..4] != [0, 0, 0] {
                return Err(form("a tree's kind is neither shelf nor source", deps_end + k * TREE));
            }
        }
        for e in 0..c {
            let file = &list.corrected[e * CORRECTED..(e + 1) * CORRECTED];
            let tree = le32(file, 0) as usize;
            if tree == 0 || tree > t {
                return Err(form("a corrected file names no tree of the table", trees_end + e * CORRECTED));
            }
            if !text_ok(file, 4) || !text_ok(file, 12) {
                return Err(form("a corrected file's string is outside the heap", trees_end + e * CORRECTED));
            }
        }
        Ok(list)
    }

    /// How many trees past the checkout the rows' paths name: tree `k` is `1..=trees()`.
    pub fn trees(&self) -> usize {
        self.trees.len() / TREE
    }

    /// Where tree `k` (1 and up) is kept, and its files' identity; none for the checkout (0) or
    /// past the table.
    pub fn tree(&self, k: usize) -> Option<TreeRow> {
        let at = k.checked_sub(1)?.checked_mul(TREE)?;
        let row = self.trees.get(at..at + TREE)?;
        let mut name = [0; 32];
        name.copy_from_slice(&row[4..36]);
        let mut files = [0; 32];
        files.copy_from_slice(&row[36..68]);
        let origin = match row[0] {
            SHELF => PackOrigin::Shelf { digest: name },
            _ => PackOrigin::Source { key: ContentId(name) },
        };
        Some(TreeRow { origin, files })
    }

    /// The files read corrected (D58), in the world's order: `(tree, path in its pack, text)`,
    /// read in place of the pack's bytes (`crate::Texts::corrected`).
    pub fn corrected(&self) -> impl Iterator<Item = (u32, &'b str, &'b str)> + '_ {
        self.corrected.chunks_exact(CORRECTED).map(|file| (le32(file, 0), self.text(file, 4), self.text(file, 12)))
    }

    fn row(&self, r: usize) -> &'b [u8] {
        &self.rows[r * ROW..(r + 1) * ROW]
    }

    fn text(&self, b: &[u8], at: usize) -> &'b str {
        let (off, len) = (le32(b, at) as usize, le32(b, at + 4) as usize);
        &self.heap[off..off + len]
    }

    /// The row of the crate `key`: a search in the bytes, nothing allocated.
    pub fn find(&self, key: CrateItemKey) -> Option<usize> {
        let (mut lo, mut hi) = (0, self.n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.row(mid)[..32].cmp(&key.0.0[..]) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    pub fn key(&self, r: usize) -> CrateItemKey {
        let mut k = [0; 32];
        k.copy_from_slice(&self.row(r)[..32]);
        CrateItemKey(ContentId(k))
    }
    pub fn root(&self, r: usize) -> &'b str {
        self.text(self.row(r), 40)
    }
    pub fn shown_from(&self, r: usize) -> &'b str {
        self.text(self.row(r), 48)
    }
    /// Its root is in the checkout (tree 0): a workspace member's crate.
    pub fn member(&self, r: usize) -> bool {
        self.row(r)[65] & MEMBER != 0
    }
    /// Each dependency: the name it calls it, and its row.
    pub fn deps_named(&self, r: usize) -> impl Iterator<Item = (&'b str, usize)> + '_ {
        let row = self.row(r);
        let (start, count) = (le32(row, 56) as usize, le32(row, 60) as usize);
        (start..start + count).map(move |e| {
            let dep = &self.deps[e * DEP..(e + 1) * DEP];
            (self.text(dep, 0), le32(dep, 8) as usize)
        })
    }
    /// The name of its `Script` part (kind byte, subject), when it has one.
    pub fn script(&self, r: usize) -> Option<[u8; 33]> {
        let row = self.row(r);
        if row[66] == 0 {
            return None;
        }
        let mut name = [0; 33];
        name[0] = row[66];
        name[1..].copy_from_slice(&row[68..100]);
        Some(name)
    }
    /// The facts of its environment as the world made it (blaze-scope `CrateEnv::facts`), its
    /// files read under `shown_from`: `CrateEnv::of` of them answers every `env!` as the
    /// world's environment of the crate does. No script record is joined (`script: None`): a
    /// reader with the crate's `Script` input passes its outputs in the facts it hands `of`.
    pub fn env_facts(&self, r: usize) -> EnvFacts<'b> {
        let row = self.row(r);
        let flags = row[ENV_AT + 40];
        EnvFacts {
            crate_name: self.text(row, ENV_AT),
            package: (flags & ENV_PACKAGE != 0).then(|| (self.text(row, ENV_AT + 8), self.text(row, ENV_AT + 16))),
            bin: (flags & ENV_BIN != 0).then(|| self.text(row, ENV_AT + 24)),
            // A member's dir was checked a plain path at `open`.
            seen: if flags & ENV_MEMBER != 0 { SeenRoot::Member { dir: self.text(row, ENV_AT + 32) } } else { SeenRoot::Package },
            read_folder: self.shown_from(r),
            build_script: flags & ENV_BUILD_SCRIPT != 0,
            script: None,
            test_or_bench: flags & ENV_TEST_OR_BENCH != 0,
        }
    }
}

/// Whether two environments' facts are one (the script record aside): `read_folder` compared
/// without its trailing `/`, as `CrateEnv::of` keeps it.
fn same_facts(a: &EnvFacts<'_>, b: &EnvFacts<'_>) -> bool {
    a.crate_name == b.crate_name
        && a.package == b.package
        && a.bin == b.bin
        && a.seen == b.seen
        && a.read_folder.trim_end_matches('/') == b.read_folder.trim_end_matches('/')
        && a.build_script == b.build_script
        && a.test_or_bench == b.test_or_bench
}

impl<'b> CrateGraph<'b> for CrateList<'b> {
    fn len(&self) -> usize {
        self.n
    }
    fn name(&self, at: usize) -> &'b str {
        self.text(self.row(at), 32)
    }
    fn side(&self, at: usize) -> Side {
        if self.row(at)[64] == 1 { Side::Macro } else { Side::Target }
    }
    fn proc_macro(&self, at: usize) -> bool {
        self.row(at)[65] & PROC_MACRO != 0
    }
    fn toolchain(&self, at: usize) -> bool {
        self.row(at)[65] & TOOLCHAIN != 0
    }
    fn refused(&self, at: usize) -> bool {
        self.row(at)[65] & REFUSED != 0
    }
    fn deps(&self, at: usize) -> impl Iterator<Item = usize> + '_ {
        self.deps_named(at).map(|(_, row)| row)
    }
    fn sysroot(&self) -> [Option<usize>; 4] {
        self.sysroot.map(|r| (r != NONE).then_some(r as usize))
    }
}

/// How many fields of `world`'s crates differ from `list`'s rows (each crate found by its key;
/// a crate with no row counts once): 0 when the list is the world's. The fields: name, root,
/// shown_from, side, proc_macro, toolchain, member, refused, the environment's facts, sysroot, each dependency's name and
/// key, each tree's origin and files, and each corrected file.
pub fn differences(world: &World<'_>, list: &CrateList<'_>) -> usize {
    let mut differ = 0;
    if list.len() != world.crates.len() {
        differ += 1;
    }
    let key_row = |at: usize| list.find(world.crates[at].input.key);
    for (at, c) in world.crates.iter().enumerate() {
        let Some(r) = key_row(at) else {
            differ += 1;
            continue;
        };
        differ += usize::from(list.name(r) != c.input.name)
            + usize::from(list.root(r) != c.input.root)
            + usize::from(list.shown_from(r) != c.input.shown_from)
            + usize::from(list.side(r) != c.side)
            + usize::from(list.proc_macro(r) != c.proc_macro)
            + usize::from(list.toolchain(r) != world.toolchain(at))
            + usize::from(list.member(r) != c.input.root.starts_with("/0/"))
            + usize::from(list.refused(r) != world.refused(at))
            + usize::from(!world.texts.crate_env(at).is_some_and(|env| same_facts(&env.facts(), &list.env_facts(r))));
        let mut deps = list.deps_named(r);
        for (name, dep) in c.input.deps.iter() {
            match deps.next() {
                Some((n, row)) if n == *name && list.key(row) == world.crates[*dep].input.key => {}
                _ => differ += 1,
            }
        }
        differ += deps.count();
    }
    let rows = list.sysroot();
    for (i, id) in CrateGraph::sysroot(world).iter().enumerate() {
        let want = id.map(|at| world.crates[at].input.key);
        if want != rows[i].map(|r| list.key(r)) {
            differ += 1;
        }
    }
    let (packs, origins) = (world.texts.packs(), world.texts.origins());
    differ += packs.len().abs_diff(list.trees());
    for (k, (pack, origin)) in packs.iter().zip(origins.iter()).enumerate() {
        if list.tree(k + 1) != Some(TreeRow { origin: *origin, files: pack.tree().0 }) {
            differ += 1;
        }
    }
    let mut corrected = list.corrected();
    for (tree, path, text) in world.texts.corrected().iter() {
        if corrected.next() != Some((*tree, *path, *text)) {
            differ += 1;
        }
    }
    differ + corrected.count()
}
