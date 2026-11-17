//! The toolchain's manifest record, written once and read in place ([`ToolchainView`]).
//! Little-endian; a text is `(offset u32, length u32)` into the strings:
//!
//! ```text
//! 0..8     b"BLZTCH03" (BLZTCH02, before the unit's sysroot byte, is refused by name)
//! 8..36    counts u32: crates, targets, units, deps, features, cfgs, bytes of strings
//! then     crates   96 each: name, version, edition, lib, root, dir (texts); role u8; pack
//!                   kind u8 (blaze-fetch's: 1 a registry crate, 3 a tree); 6 zero; pack
//!                   length u64; pack digest [32] (the pack's footer)
//! then     targets  24 each: triple (text), first unit u32, units u32, first cfg u32, cfgs u32
//! then     units    32 each: crate u32, kind u8 (1 lib, 2 proc macro, 3 build script), for the
//!                   host u8, implicit sysroot u8 (1 none, 2 core, 3 std: a fact of the unit's
//!                   root text, crate::implied), 1 zero, root (text), first feature u32,
//!                   features u32, first dep u32, deps u32. A target-side library unit's deps
//!                   include the sysroot crates its root names (`core`, `std`, `alloc`) as
//!                   edges like any other, so a reader's closure walk opens them by itself.
//! then     deps     12 each: extern name (text), unit u32 (within its target)
//! then     features  8 each: a text
//! then     cfgs     16 each: name (text), value (text; offset u32::MAX and length 0: none).
//!                   A target's cfg atoms as the pinned nightly prints them (its recorded
//!                   answers: `cfg`, `nightly_cfg`, `panic`), sorted, each once; a unit's are
//!                   these and `feature = ".."` for each of its features.
//! then     the strings
//! last 32  the BLAKE3 of every byte before it: the toolchain's id
//! ```
//!
//! A crate's files are in its pack under `dir` (`core` in the library/ pack, `crates/blaze-std`
//! in the workspace's, empty for a registry crate's own pack); `root` is relative to `dir`.
//! Many crates may share one pack. Crates are in the order the tool gives them; a target's units
//! are in blaze-unit-graph's order. Two writers of one input write equal bytes (rule 25).

use blaze_fetch::Blake3;
use blaze_own::{AVec, Alloc, AllocFailure};

use crate::ToolchainId;
use crate::implied::ImplicitSysroot;

pub const MAGIC: &[u8; 8] = b"BLZTCH03";
const HEADER: usize = 36;
const CRATE: usize = 96;
const TARGET: usize = 24;
const UNIT: usize = 32;
const DEP: usize = 12;
const FEATURE: usize = 8;
const CFG: usize = 16;
const FOOTER: usize = 32;
/// A cfg's value offset when it has none.
const NO_VALUE: u32 = u32::MAX;

/// What a toolchain crate is to the units that are given it (toolchain.md, section 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Role {
    Core = 1,
    Alloc = 2,
    /// Rust's `std`.
    Std = 3,
    ProcMacro = 4,
    /// blaze's standard library: `std` under a blaze edition.
    Bstd = 5,
    /// A crate of library/ beneath core, alloc or std (a vendored copy included).
    Library = 6,
    /// A crate of blaze's workspace beneath bstd.
    Blaze = 7,
    /// A registry crate beneath bstd, its pack the one `Fetch` would store.
    Registry = 8,
}

impl Role {
    fn of(byte: u8) -> Option<Role> {
        [Role::Core, Role::Alloc, Role::Std, Role::ProcMacro, Role::Bstd, Role::Library, Role::Blaze, Role::Registry]
            .into_iter()
            .find(|role| *role as u8 == byte)
    }
}

/// What a unit of the sysroot builds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum UnitKind {
    Lib = 1,
    ProcMacro = 2,
    BuildScript = 3,
}

impl UnitKind {
    fn of(byte: u8) -> Option<UnitKind> {
        [UnitKind::Lib, UnitKind::ProcMacro, UnitKind::BuildScript].into_iter().find(|kind| *kind as u8 == byte)
    }
}

/// One crate, as given to [`write`].
#[derive(Clone, Copy, Debug)]
pub struct CrateIn<'s> {
    pub name: &'s str,
    pub version: &'s str,
    /// As the manifest writes it (`2021`, `2024`, `blaze-2026.11`).
    pub edition: &'s str,
    /// Its library's crate name.
    pub lib: &'s str,
    /// Its library's root file, relative to `dir`.
    pub root: &'s str,
    /// The package's folder within its pack, empty for the pack's top.
    pub dir: &'s str,
    pub role: Role,
    pub pack_kind: u8,
    pub pack_len: u64,
    pub pack: [u8; 32],
}

/// One unit of a target's sysroot graph, as given to [`write`].
#[derive(Clone, Copy, Debug)]
pub struct UnitIn<'s> {
    /// The crate's index among the crates.
    pub krate: u32,
    pub kind: UnitKind,
    pub for_host: bool,
    /// Its root file, relative to its crate's `dir`.
    pub root: &'s str,
    pub features: &'s [&'s str],
    /// (extern name, unit index within the same target), the implied sysroot edges included.
    pub deps: &'s [(&'s str, u32)],
    /// What its root's text gives it from the sysroot (crate::implied::crate_facts).
    pub sysroot: ImplicitSysroot,
}

#[derive(Clone, Copy, Debug)]
pub struct TargetIn<'s> {
    pub triple: &'s str,
    pub units: &'s [UnitIn<'s>],
    /// The target's cfg atoms, (name, value), sorted and each once.
    pub cfg: &'s [(&'s str, Option<&'s str>)],
}

/// Why bytes are not a toolchain manifest, or a manifest could not be made.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RecordRefused {
    /// The part that is wrong.
    Form { why: &'static str },
    /// Its BLAKE3 is not the id it was opened as.
    NotTheId { expected: ToolchainId, found: ToolchainId },
    /// A manifest of an earlier form (`found` its first eight bytes): cut again.
    OldForm { found: [u8; 8] },
    Alloc(AllocFailure),
    /// More than `u32` of something.
    TooLarge,
}

impl core::fmt::Display for RecordRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RecordRefused::Form { why } => write!(f, "not a toolchain manifest: {why}"),
            RecordRefused::NotTheId { expected, found } => write!(f, "its BLAKE3 is {found}, not the toolchain id {expected}"),
            RecordRefused::OldForm { found } => write!(
                f,
                "a toolchain manifest of form {}, cut by an earlier toolchain_cut (before each unit's implicit sysroot was recorded); this blaze reads {}: cut the toolchain again",
                core::str::from_utf8(found).unwrap_or("(not text)"),
                core::str::from_utf8(MAGIC).unwrap_or("(not text)")
            ),
            RecordRefused::Alloc(failure) => write!(f, "{failure}"),
            RecordRefused::TooLarge => f.write_str("a toolchain manifest past 4 GiB of a part"),
        }
    }
}

impl core::error::Error for RecordRefused {}

fn u32_of(n: usize) -> Result<u32, RecordRefused> {
    u32::try_from(n).map_err(|_| RecordRefused::TooLarge)
}

/// The strings being gathered, and each text's reference to them.
struct Strings<'a>(AVec<'a, u8>);

impl Strings<'_> {
    fn text(&mut self, s: &str) -> Result<[u8; 8], RecordRefused> {
        let at = u32_of(self.0.len())?;
        self.0.try_reserve(s.len()).map_err(RecordRefused::Alloc)?;
        self.0.extend_from_slice(s.as_bytes());
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&at.to_le_bytes());
        out[4..].copy_from_slice(&u32_of(s.len())?.to_le_bytes());
        Ok(out)
    }
}

/// The record of `crates` and `targets`, in `alloc`.
pub fn write<'a>(crates: &[CrateIn<'_>], targets: &[TargetIn<'_>], alloc: Alloc<'a>) -> Result<AVec<'a, u8>, RecordRefused> {
    let all_units = || targets.iter().flat_map(|t| t.units.iter());
    let units = all_units().count();
    let deps: usize = all_units().map(|u| u.deps.len()).sum();
    let features: usize = all_units().map(|u| u.features.len()).sum();
    let cfgs: usize = targets.iter().map(|t| t.cfg.len()).sum();
    for t in targets {
        if !t.cfg.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(RecordRefused::Form { why: "a target's cfg atoms not sorted, each once" });
        }
    }
    let mut strings = Strings(AVec::new_in(alloc));
    let mut body: AVec<'a, u8> = AVec::new_in(alloc);
    let fixed = HEADER + crates.len() * CRATE + targets.len() * TARGET + units * UNIT + deps * DEP + features * FEATURE + cfgs * CFG;
    body.try_reserve(fixed).map_err(RecordRefused::Alloc)?;
    body.extend_from_slice(MAGIC);
    for count in [crates.len(), targets.len(), units, deps, features, cfgs] {
        body.extend_from_slice(&u32_of(count)?.to_le_bytes());
    }
    // The strings' length is written once they are all known.
    body.extend_from_slice(&[0; 4]);
    for c in crates {
        for s in [c.name, c.version, c.edition, c.lib, c.root, c.dir] {
            body.extend_from_slice(&strings.text(s)?);
        }
        body.extend_from_slice(&[c.role as u8, c.pack_kind, 0, 0, 0, 0, 0, 0]);
        body.extend_from_slice(&c.pack_len.to_le_bytes());
        body.extend_from_slice(&c.pack);
    }
    let (mut first_unit, mut first_cfg) = (0usize, 0usize);
    for t in targets {
        body.extend_from_slice(&strings.text(t.triple)?);
        for n in [first_unit, t.units.len(), first_cfg, t.cfg.len()] {
            body.extend_from_slice(&u32_of(n)?.to_le_bytes());
        }
        first_unit += t.units.len();
        first_cfg += t.cfg.len();
    }
    let (mut first_dep, mut first_feature) = (0usize, 0usize);
    for u in all_units() {
        if u.krate as usize >= crates.len() {
            return Err(RecordRefused::Form { why: "a unit of no crate" });
        }
        body.extend_from_slice(&u.krate.to_le_bytes());
        body.extend_from_slice(&[u.kind as u8, u8::from(u.for_host), u.sysroot as u8, 0]);
        body.extend_from_slice(&strings.text(u.root)?);
        for n in [first_feature, u.features.len(), first_dep, u.deps.len()] {
            body.extend_from_slice(&u32_of(n)?.to_le_bytes());
        }
        first_dep += u.deps.len();
        first_feature += u.features.len();
    }
    for t in targets {
        for (name, unit) in t.units.iter().flat_map(|u| u.deps.iter()) {
            if *unit as usize >= t.units.len() {
                return Err(RecordRefused::Form { why: "a dependency on no unit of its target" });
            }
            body.extend_from_slice(&strings.text(name)?);
            body.extend_from_slice(&unit.to_le_bytes());
        }
    }
    for feature in all_units().flat_map(|u| u.features.iter()) {
        body.extend_from_slice(&strings.text(feature)?);
    }
    for (name, value) in targets.iter().flat_map(|t| t.cfg.iter()) {
        body.extend_from_slice(&strings.text(name)?);
        match value {
            Some(value) => body.extend_from_slice(&strings.text(value)?),
            None => {
                body.extend_from_slice(&NO_VALUE.to_le_bytes());
                body.extend_from_slice(&[0; 4]);
            }
        }
    }
    let strings = strings.0;
    body[32..36].copy_from_slice(&u32_of(strings.len())?.to_le_bytes());
    body.try_reserve(strings.len() + FOOTER).map_err(RecordRefused::Alloc)?;
    body.extend_from_slice(&strings);
    let digest = Blake3::of(&body).0;
    body.extend_from_slice(&digest);
    Ok(body)
}

/// A toolchain manifest read in place, its form, ranges and id checked once ([`ToolchainView::open`]).
#[derive(Clone, Copy)]
pub struct ToolchainView<'b> {
    bytes: &'b [u8],
    counts: [usize; 7],
    at: [usize; 7],
}

/// One crate of the toolchain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CrateRow<'b> {
    pub index: u32,
    pub name: &'b str,
    pub version: &'b str,
    pub edition: &'b str,
    pub lib: &'b str,
    pub root: &'b str,
    pub dir: &'b str,
    pub role: Role,
    pub pack_kind: u8,
    pub pack_len: u64,
    pub pack: [u8; 32],
}

/// One unit of a target's sysroot graph.
#[derive(Clone, Copy)]
pub struct UnitRow<'b> {
    view: ToolchainView<'b>,
    pub krate: u32,
    pub kind: UnitKind,
    pub for_host: bool,
    /// What its root's text gives it from the sysroot (its edges are among [`UnitRow::deps`]).
    pub sysroot: ImplicitSysroot,
    pub root: &'b str,
    features: (usize, usize),
    deps: (usize, usize),
}

const CRATES: usize = 0;
const TARGETS: usize = 1;
const UNITS: usize = 2;
const DEPS: usize = 3;
const FEATURES: usize = 4;
const CFGS: usize = 5;
const STRINGS: usize = 6;

fn le32(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes")) as usize
}

impl<'b> ToolchainView<'b> {
    /// The id a record of `bytes` is named by: the BLAKE3 of all but its last 32 bytes. Nothing
    /// is checked; [`ToolchainView::open`] checks.
    pub fn id_of_bytes(bytes: &[u8]) -> ToolchainId {
        ToolchainId(Blake3::of(&bytes[..bytes.len().saturating_sub(FOOTER)]).0)
    }

    /// `bytes` as the manifest of toolchain `id`: its length, its BLAKE3 against `id`, and every
    /// text, crate, unit and dependency range checked, once.
    pub fn open(bytes: &'b [u8], id: ToolchainId) -> Result<ToolchainView<'b>, RecordRefused> {
        let form = |why| RecordRefused::Form { why };
        if bytes.len() < HEADER + FOOTER {
            return Err(form("no header"));
        }
        if &bytes[..8] != MAGIC {
            if bytes[..6] == MAGIC[..6] {
                return Err(RecordRefused::OldForm { found: bytes[..8].try_into().expect("8 bytes") });
            }
            return Err(form("no header"));
        }
        let body = bytes.len() - FOOTER;
        let found = ToolchainId(Blake3::of(&bytes[..body]).0);
        if found.0 != bytes[body..] {
            return Err(form("its footer is not the BLAKE3 of its bytes"));
        }
        if found != id {
            return Err(RecordRefused::NotTheId { expected: id, found });
        }
        let mut counts = [0usize; 7];
        for (k, count) in counts.iter_mut().enumerate() {
            *count = le32(bytes, 8 + k * 4);
        }
        let sizes = [CRATE, TARGET, UNIT, DEP, FEATURE, CFG, 1];
        let mut at = [0usize; 7];
        let mut next = HEADER as u64;
        for k in 0..7 {
            at[k] = next as usize;
            next += counts[k] as u64 * sizes[k] as u64;
        }
        if next != body as u64 {
            return Err(form("counts that do not add up to its size"));
        }
        let view = ToolchainView { bytes, counts, at };
        for c in 0..counts[CRATES] {
            let row = at[CRATES] + c * CRATE;
            for s in 0..6 {
                view.text(row + s * 8).ok_or(form("a crate's text outside the strings"))?;
            }
            if Role::of(bytes[row + 48]).is_none() || bytes[row + 50..row + 56] != [0; 6] {
                return Err(form("an unknown role"));
            }
        }
        let (mut units_seen, mut cfgs_seen) = (0, 0);
        for t in 0..counts[TARGETS] {
            let row = at[TARGETS] + t * TARGET;
            view.text(row).ok_or(form("a triple outside the strings"))?;
            let (first, n) = (le32(bytes, row + 8), le32(bytes, row + 12));
            if first != units_seen || first + n > counts[UNITS] {
                return Err(form("a target's units out of order"));
            }
            units_seen += n;
            let (cfg_first, cfg_n) = (le32(bytes, row + 16), le32(bytes, row + 20));
            if cfg_first != cfgs_seen || cfg_first + cfg_n > counts[CFGS] {
                return Err(form("a target's cfgs out of order"));
            }
            cfgs_seen += cfg_n;
            let mut last: Option<(&str, Option<&str>)> = None;
            for c in cfg_first..cfg_first + cfg_n {
                let atom = view.cfg_row(at[CFGS] + c * CFG).ok_or(form("a cfg outside the strings"))?;
                if last.is_some_and(|last| last >= atom) {
                    return Err(form("a target's cfg atoms not sorted, each once"));
                }
                last = Some(atom);
            }
            for u in first..first + n {
                let unit = at[UNITS] + u * UNIT;
                if le32(bytes, unit) >= counts[CRATES] {
                    return Err(form("a unit of no crate"));
                }
                if UnitKind::of(bytes[unit + 4]).is_none() || bytes[unit + 5] > 1 || ImplicitSysroot::of(bytes[unit + 6]).is_none() || bytes[unit + 7] != 0 {
                    return Err(form("an unknown unit kind"));
                }
                view.text(unit + 8).ok_or(form("a unit's root outside the strings"))?;
                let (ff, fnum, df, dnum) = (le32(bytes, unit + 16), le32(bytes, unit + 20), le32(bytes, unit + 24), le32(bytes, unit + 28));
                if ff + fnum > counts[FEATURES] || df + dnum > counts[DEPS] {
                    return Err(form("a unit's ranges outside the record"));
                }
                for d in df..df + dnum {
                    let dep = at[DEPS] + d * DEP;
                    view.text(dep).ok_or(form("a dependency's name outside the strings"))?;
                    if le32(bytes, dep + 8) >= n {
                        return Err(form("a dependency on no unit of its target"));
                    }
                }
                for f in ff..ff + fnum {
                    view.text(at[FEATURES] + f * FEATURE).ok_or(form("a feature outside the strings"))?;
                }
            }
        }
        if units_seen != counts[UNITS] {
            return Err(form("units no target holds"));
        }
        if cfgs_seen != counts[CFGS] {
            return Err(form("cfgs no target holds"));
        }
        Ok(view)
    }

    /// The cfg atom whose row is at `row`: `None` when a text is outside the strings, or a
    /// missing value is not written as exactly `(u32::MAX, 0)`.
    fn cfg_row(&self, row: usize) -> Option<(&'b str, Option<&'b str>)> {
        let name = self.text(row)?;
        if le32(self.bytes, row + 8) == NO_VALUE as usize {
            return (le32(self.bytes, row + 12) == 0).then_some((name, None));
        }
        Some((name, Some(self.text(row + 8)?)))
    }

    /// The cfg atoms of `triple` (name, value), sorted: the target's own, before a unit's
    /// `feature = ".."`s are added. `None` when the toolchain was not cut for it.
    pub fn cfg(&self, triple: &str) -> Option<impl Iterator<Item = (&'b str, Option<&'b str>)> + use<'b>> {
        let t = (0..self.counts[TARGETS]).find(|t| self.text_checked(self.at[TARGETS] + t * TARGET) == triple)?;
        let row = self.at[TARGETS] + t * TARGET;
        let (first, n) = (le32(self.bytes, row + 16), le32(self.bytes, row + 20));
        let view = *self;
        Some((first..first + n).map(move |c| view.cfg_row(view.at[CFGS] + c * CFG).expect("checked at open")))
    }

    /// The text whose reference is at `row`.
    fn text(&self, row: usize) -> Option<&'b str> {
        let (at, len) = (le32(self.bytes, row), le32(self.bytes, row + 4));
        let end = at.checked_add(len)?;
        if end > self.counts[STRINGS] {
            return None;
        }
        core::str::from_utf8(&self.bytes[self.at[STRINGS] + at..self.at[STRINGS] + end]).ok()
    }

    fn text_checked(&self, row: usize) -> &'b str {
        self.text(row).expect("checked at open")
    }

    pub fn crate_count(&self) -> u32 {
        self.counts[CRATES] as u32
    }

    pub fn crates(&self) -> impl Iterator<Item = CrateRow<'b>> + '_ {
        (0..self.counts[CRATES]).map(|c| self.krate(c as u32))
    }

    /// The crate at `index`, which must be below [`ToolchainView::crate_count`].
    pub fn krate(&self, index: u32) -> CrateRow<'b> {
        let row = self.at[CRATES] + index as usize * CRATE;
        let b = self.bytes;
        CrateRow {
            index,
            name: self.text_checked(row),
            version: self.text_checked(row + 8),
            edition: self.text_checked(row + 16),
            lib: self.text_checked(row + 24),
            root: self.text_checked(row + 32),
            dir: self.text_checked(row + 40),
            role: Role::of(b[row + 48]).expect("checked at open"),
            pack_kind: b[row + 49],
            pack_len: u64::from_le_bytes(b[row + 56..row + 64].try_into().expect("8 bytes")),
            pack: b[row + 64..row + 96].try_into().expect("32 bytes"),
        }
    }

    /// The first crate of `role`: the toolchain's `core`, `std` or bstd.
    pub fn with_role(&self, role: Role) -> Option<CrateRow<'b>> {
        self.crates().find(|c| c.role == role)
    }

    /// The triples it has a sysroot graph for.
    pub fn triples(&self) -> impl Iterator<Item = &'b str> + '_ {
        (0..self.counts[TARGETS]).map(|t| self.text_checked(self.at[TARGETS] + t * TARGET))
    }

    /// The sysroot's units for `triple`, in blaze-unit-graph's order; `None` when the toolchain
    /// was not cut for it.
    pub fn units(&self, triple: &str) -> Option<impl Iterator<Item = UnitRow<'b>> + use<'b>> {
        let t = (0..self.counts[TARGETS]).find(|t| self.text_checked(self.at[TARGETS] + t * TARGET) == triple)?;
        let row = self.at[TARGETS] + t * TARGET;
        let (first, n) = (le32(self.bytes, row + 8), le32(self.bytes, row + 12));
        let view = *self;
        Some((first..first + n).map(move |u| {
            let unit = view.at[UNITS] + u * UNIT;
            let b = view.bytes;
            UnitRow {
                view,
                krate: le32(b, unit) as u32,
                kind: UnitKind::of(b[unit + 4]).expect("checked at open"),
                for_host: b[unit + 5] == 1,
                sysroot: ImplicitSysroot::of(b[unit + 6]).expect("checked at open"),
                root: view.text_checked(unit + 8),
                features: (le32(b, unit + 16), le32(b, unit + 20)),
                deps: (le32(b, unit + 24), le32(b, unit + 28)),
            }
        }))
    }
}

impl<'b> UnitRow<'b> {
    pub fn features(&self) -> impl Iterator<Item = &'b str> + use<'b> {
        let (first, n) = self.features;
        let view = self.view;
        (first..first + n).map(move |f| view.text_checked(view.at[FEATURES] + f * FEATURE))
    }

    /// (extern name, unit index within the target).
    pub fn deps(&self) -> impl Iterator<Item = (&'b str, u32)> + use<'b> {
        let (first, n) = self.deps;
        let view = self.view;
        (first..first + n).map(move |d| {
            let row = view.at[DEPS] + d * DEP;
            (view.text_checked(row), le32(view.bytes, row + 8) as u32)
        })
    }
}
