//! The unit graph as a record (docs/v2/plan/design/units.md, "The record").
//!
//! - [`encode`]: the graph's bytes, a function of the graph alone: units in their order (by
//!   package, kind and configuration), each list sorted, every number little-endian, every text
//!   length-prefixed. Two builders of one graph write equal bytes (rule 25), and nothing of what
//!   was read to make it (a manifest's comments, its line numbers, a key) is in them: an edit
//!   that does not change resolution leaves the record byte-equal, so everything keyed by its
//!   digest downstream is cut off early. The one exception is each package's `CARGO_PKG_*`
//!   metadata ([`UnitGraph::meta`]), which crates read: an `authors` edit changes the bytes,
//!   and a crate's own tasks rerun only where a read list holds a value that moved.
//! - [`decode`]: the bytes back, every length and index checked once; `encode(decode(b)) == b`.
//! - [`Source`]: the record's source digest, what it read, in the order read: the lock's bytes
//!   (which pin every dependency's sources, so their manifests are covered by it), each
//!   workspace manifest and listing the layout read, the target and host descriptions, the ask,
//!   and the toolchain's id when the sysroot's units are joined. The driver keys the task by it
//!   (its key is H(tool, kind, subject, source)); the record is valid by that key.

#![blaze::sans_io]

use core::fmt;

use blaze_ids::{ContentId, Hasher};
use blaze_manifest::layout::Entry;
use blaze_manifest::lock::LockId;
use blaze_own::{AVec, Alloc, String, Vec};
use edition::{Edition, PackageEdition};

use crate::resolve::unit_cfg;
use crate::{Ask, CfgAtom, ExtraUnit, LeftOut, LeftOutKind, PackageMeta, Resolver, TargetDesc, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};

/// The record's form. A change to it is a new magic; an older one is refused by it.
/// `BUG2`: each package's `CARGO_PKG_*` metadata after the unavailable ids. `BUG3`: no
/// unavailable ids and no `incomplete` flag: a graph is whole or not made (states.md 35).
/// `BUG4`: the targets left out by `required-features` after the metadata.
pub const MAGIC: &[u8; 4] = b"BUG4";
/// The domain of [`Source`]'s digest.
pub const SOURCE_DOMAIN: &str = "blaze-unit-graph: what a request's unit graph read, v1";

/// A unit whose cfg set is not the one its platform, features and kind make: the graph was
/// not made by [`crate::Resolution`] for these platforms, and its record would not decode to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotItsCfg {
    pub unit: UnitId,
}

impl fmt::Display for NotItsCfg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unit {}'s cfg set is not the one its platform, features and kind make", self.unit.0)
    }
}

/// The graph's bytes. Each unit's cfg set is not written: it is its platform's atoms (the two
/// platforms are written once), its features and its kind's flags, remade on decode; a unit
/// whose cfg is other than that is [`NotItsCfg`].
pub fn encode<'g, 'r>(graph: &UnitGraph<'g>, target: &TargetDesc<'g>, host: &TargetDesc<'g>, alloc: Alloc<'r>) -> Result<AVec<'r, u8>, NotItsCfg> {
    let mut w = Writer(Vec::with_capacity_in(graph.units.len().saturating_mul(256).max(1024), alloc));
    w.0.extend_from_slice(MAGIC);
    w.u8(match graph.resolver {
        Resolver::V1 => 1,
        Resolver::V2 => 2,
        Resolver::V3 => 3,
    });
    for desc in [target, host] {
        w.text(&desc.triple);
        w.u32(desc.cfg.len() as u32);
        for atom in desc.cfg.iter() {
            w.atom(atom);
        }
    }
    w.u32(graph.units.len() as u32);
    let target_sorted = target.cfg.windows(2).all(|p| p[0] <= p[1]);
    let host_sorted = host.cfg.windows(2).all(|p| p[0] <= p[1]);
    for (at, unit) in graph.units.iter().enumerate() {
        let (desc, desc_sorted) = if unit.for_host { (host, host_sorted) } else { (target, target_sorted) };
        if !cfg_is(unit, desc, desc_sorted) {
            return Err(NotItsCfg { unit: UnitId(at as u32) });
        }
        w.u32(unit.package.0);
        w.text(&unit.name);
        w.text(&unit.version);
        match &unit.kind {
            UnitKind::Lib => w.u8(0),
            UnitKind::ProcMacro => w.u8(1),
            UnitKind::Bin(name) => {
                w.u8(2);
                w.text(name);
            }
            UnitKind::BuildScript => w.u8(3),
            UnitKind::Test { bin: None } => w.u8(4),
            UnitKind::Test { bin: Some(name) } => {
                w.u8(5);
                w.text(name);
            }
            // The target's name, then its flags and required features: all in the record, so
            // two targets of one package, or one whose `harness` changed, are two records.
            UnitKind::IntegrationTest(extra) | UnitKind::Example(extra) | UnitKind::Bench(extra) => {
                w.u8(match &unit.kind {
                    UnitKind::IntegrationTest(_) => 6,
                    UnitKind::Example(_) => 7,
                    _ => 8,
                });
                w.text(&extra.name);
                w.u8(u8::from(extra.harness) | (u8::from(extra.test) << 1));
                w.u32(extra.required_features.len() as u32);
                for feature in extra.required_features.iter() {
                    w.text(feature);
                }
            }
        }
        w.u8(u8::from(unit.for_host));
        match unit.edition.blaze {
            Some(blaze) => {
                w.u8(1);
                w.text(blaze.manifest());
            }
            None => {
                w.u8(0);
                w.u16(unit.edition.syntax.number() as u16);
            }
        }
        w.text(&unit.crate_name);
        w.text(&unit.root);
        w.u32(unit.features.len() as u32);
        for feature in unit.features.iter() {
            w.text(feature);
        }
        w.u32(unit.deps.len() as u32);
        for dep in unit.deps.iter() {
            w.text(&dep.name);
            w.u32(dep.unit.0);
        }
        w.u32(unit.build_script.map_or(u32::MAX, |id| id.0));
        match &unit.links {
            None => w.u8(0),
            Some(links) => {
                w.u8(1);
                w.text(links);
            }
        }
    }
    // Each package's metadata (`CARGO_PKG_*`), by id: what a crate's environment reads.
    w.u32(graph.meta.len() as u32);
    for (id, meta) in graph.meta.iter() {
        w.u32(id.0);
        for value in meta.values() {
            w.text(value);
        }
    }
    // The targets left out by `required-features`, in the graph's order.
    w.u32(graph.left_out.len() as u32);
    for left in graph.left_out.iter() {
        w.u32(left.package.0);
        w.text(&left.package_name);
        w.u8(match left.kind {
            LeftOutKind::Bin => 0,
            LeftOutKind::Test => 1,
            LeftOutKind::Example => 2,
            LeftOutKind::Bench => 3,
        });
        w.text(&left.name);
        w.u32(left.off.len() as u32);
        for feature in left.off.iter() {
            w.text(feature);
        }
    }
    Ok(w.0)
}

fn is_test(kind: &UnitKind<'_>) -> bool {
    crate::extra::has_test_cfg(kind)
}

/// Whether `unit`'s cfg is the exact sorted, deduplicated set [`crate::resolve::unit_cfg`] makes
/// on `desc`, verified in-place with zero heap allocations.
fn cfg_is<'a>(unit: &Unit<'a>, desc: &TargetDesc<'a>, desc_sorted: bool) -> bool {
    // `unit_cfg` always produces strictly sorted, deduplicated atoms.
    if !unit.cfg.windows(2).all(|pair| pair[0] < pair[1]) {
        return false;
    }
    let test = is_test(&unit.kind);
    let unwinds = crate::resolve::ignores_profile_panic(&unit.kind);
    let proc_macro = unit.kind == UnitKind::ProcMacro;
    let has_unit_atom = |name: &str, value: Option<&str>| {
        unit.cfg
            .binary_search_by(|atom| atom.name.as_str().cmp(name).then_with(|| atom.value.as_deref().cmp(&value)))
            .is_ok()
    };
    let has_desc_atom = |atom: &CfgAtom<'_>| {
        if desc_sorted {
            desc.cfg.binary_search(atom).is_ok()
        } else {
            desc.cfg.iter().any(|candidate| candidate == atom)
        }
    };
    // 1. Every atom `unit_cfg` emits must be present in `unit.cfg`.
    for atom in desc.cfg.iter() {
        if unwinds && atom.name.as_str() == crate::resolve::PANIC_KEY {
            continue;
        }
        if !has_unit_atom(atom.name.as_str(), atom.value.as_deref()) {
            return false;
        }
    }
    if unwinds && !has_unit_atom(crate::resolve::PANIC_KEY, Some("unwind")) {
        return false;
    }
    for feature in unit.features.iter() {
        if !has_unit_atom("feature", Some(feature.as_str())) {
            return false;
        }
    }
    if !has_unit_atom("debug_assertions", None) {
        return false;
    }
    if test && !has_unit_atom("test", None) {
        return false;
    }
    if proc_macro && !has_unit_atom("proc_macro", None) {
        return false;
    }
    // 2. Every atom present in `unit.cfg` must be justified by one of the sources above.
    let features_sorted = unit.features.windows(2).all(|pair| pair[0] <= pair[1]);
    for atom in unit.cfg.iter() {
        let name = atom.name.as_str();
        let val = atom.value.as_deref();
        let expected = (name == "debug_assertions" && val.is_none())
            || (test && name == "test" && val.is_none())
            || (proc_macro && name == "proc_macro" && val.is_none())
            || (unwinds && name == crate::resolve::PANIC_KEY && val == Some("unwind"))
            || (name == "feature"
                && val.is_some_and(|f| {
                    if features_sorted {
                        unit.features.binary_search_by(|feat| feat.as_str().cmp(f)).is_ok()
                    } else {
                        unit.features.iter().any(|feat| feat.as_str() == f)
                    }
                }))
            || (!(unwinds && name == crate::resolve::PANIC_KEY) && has_desc_atom(atom));
        if !expected {
            return false;
        }
    }
    true
}

/// Bytes that are not a unit graph's record: where, and what is wrong there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecordRefused {
    pub at: usize,
    pub why: &'static str,
}

impl fmt::Display for RecordRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a unit graph's record at byte {}: {}", self.at, self.why)
    }
}

/// A record read back: the graph and the two platforms it was made for.
pub struct Decoded<'a> {
    pub graph: UnitGraph<'a>,
    pub target: TargetDesc<'a>,
    pub host: TargetDesc<'a>,
}

/// The graph of a record's bytes, every length, tag and unit index checked.
pub fn decode<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Result<Decoded<'a>, RecordRefused> {
    let mut r = Reader { bytes, at: 0, alloc };
    if r.take(4)? != MAGIC {
        return Err(RecordRefused { at: 0, why: "another magic (an older form, or not a record)" });
    }
    let resolver = match r.u8()? {
        1 => Resolver::V1,
        2 => Resolver::V2,
        3 => Resolver::V3,
        _ => return r.refuse("no such resolver"),
    };
    let target = r.platform()?;
    let host = r.platform()?;
    let n = r.count()?;
    let mut units = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        let package = LockId(r.u32()?);
        let name = r.text()?;
        let version = r.text()?;
        let kind = match r.u8()? {
            0 => UnitKind::Lib,
            1 => UnitKind::ProcMacro,
            2 => UnitKind::Bin(r.text()?),
            3 => UnitKind::BuildScript,
            4 => UnitKind::Test { bin: None },
            5 => UnitKind::Test { bin: Some(r.text()?) },
            tag @ 6..=8 => {
                let name = r.text()?;
                let flags = r.u8()?;
                if flags > 3 {
                    return r.refuse("unknown target flags");
                }
                let nr = r.count()?;
                let mut required_features = Vec::with_capacity_in(nr, alloc);
                for _ in 0..nr {
                    required_features.push(r.text()?);
                }
                let extra = ExtraUnit { name, harness: flags & 1 != 0, test: flags & 2 != 0, required_features };
                match tag {
                    6 => UnitKind::IntegrationTest(extra),
                    7 => UnitKind::Example(extra),
                    _ => UnitKind::Bench(extra),
                }
            }
            _ => return r.refuse("no such unit kind"),
        };
        let flags = r.u8()?;
        if flags > 1 {
            return r.refuse("unknown unit flags");
        }
        let edition = r.edition()?;
        let crate_name = r.text()?;
        let root = r.text()?;
        let nf = r.count()?;
        let mut features = Vec::with_capacity_in(nf, alloc);
        for _ in 0..nf {
            features.push(r.text()?);
        }
        let for_host = flags & 1 != 0;
        let desc = if for_host { &host } else { &target };
        let cfg = unit_cfg(desc, &features, is_test(&kind), crate::resolve::ignores_profile_panic(&kind), kind == UnitKind::ProcMacro, alloc);
        let nd = r.count()?;
        let mut deps = Vec::with_capacity_in(nd, alloc);
        for _ in 0..nd {
            let name = r.text()?;
            let unit = r.unit(n)?;
            deps.push(UnitDep { name, unit });
        }
        let build_script = match r.u32()? {
            u32::MAX => None,
            id if (id as usize) < n => Some(UnitId(id)),
            _ => return r.refuse("a build script past the last unit"),
        };
        let links = match r.u8()? {
            0 => None,
            1 => Some(r.text()?),
            _ => return r.refuse("a links tag"),
        };
        units.push(Unit {
            package,
            name,
            version,
            kind,
            for_host,
            edition,
            crate_name,
            root,
            features,
            cfg,
            deps,
            build_script,
            links,
        });
    }
    let nm = r.count()?;
    let mut meta: AVec<'a, (LockId, PackageMeta<'a>)> = Vec::with_capacity_in(nm, alloc);
    for _ in 0..nm {
        let id = LockId(r.u32()?);
        if meta.last().is_some_and(|(last, _)| *last >= id) {
            return r.refuse("package metadata not strictly by id");
        }
        let mut values: [Text<'a>; 8] = core::array::from_fn(|_| String::new_in(alloc));
        for value in values.iter_mut() {
            *value = r.text()?;
        }
        meta.push((id, PackageMeta::new(values)));
    }
    let nl = r.count()?;
    let mut left_out: AVec<'a, LeftOut<'a>> = Vec::with_capacity_in(nl, alloc);
    for _ in 0..nl {
        let package = LockId(r.u32()?);
        let package_name = r.text()?;
        let kind = match r.u8()? {
            0 => LeftOutKind::Bin,
            1 => LeftOutKind::Test,
            2 => LeftOutKind::Example,
            3 => LeftOutKind::Bench,
            _ => return r.refuse("no such left-out target kind"),
        };
        let name = r.text()?;
        let no = r.count()?;
        if no == 0 {
            return r.refuse("a target left out with no feature off");
        }
        let mut off = Vec::with_capacity_in(no, alloc);
        for _ in 0..no {
            off.push(r.text()?);
        }
        if left_out.last().is_some_and(|last: &LeftOut<'a>| (last.package, last.kind, last.name.as_str()) >= (package, kind, name.as_str())) {
            return r.refuse("left-out targets not strictly by (package, kind, name)");
        }
        left_out.push(LeftOut { package, package_name, kind, name, off });
    }
    if r.at != bytes.len() {
        return r.refuse("bytes after the record");
    }
    Ok(Decoded { graph: UnitGraph { units, resolver, meta, left_out }, target, host })
}

/// Borrowed target kind returned by [`scan_units`] with zero string or cfg allocations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScannedKind<'b> {
    Lib,
    ProcMacro,
    Bin(&'b str),
    BuildScript,
    Test { bin: Option<&'b str> },
    IntegrationTest { name: &'b str, harness: bool, test: bool, required_features_met: bool },
    Example { name: &'b str, harness: bool, test: bool, required_features_met: bool },
    Bench { name: &'b str, harness: bool, test: bool, required_features_met: bool },
}

impl ScannedKind<'_> {
    /// Whether this unit kind carries `#[cfg(test)]`, matching `extra::has_test_cfg`.
    pub const fn has_test_cfg(&self) -> bool {
        match *self {
            ScannedKind::Test { .. } => true,
            ScannedKind::IntegrationTest { harness, .. } | ScannedKind::Bench { harness, .. } => harness,
            ScannedKind::Lib | ScannedKind::ProcMacro | ScannedKind::Bin(_) | ScannedKind::BuildScript | ScannedKind::Example { .. } => false,
        }
    }
}

/// Lightweight zero-copy view of one unit decoded by [`scan_units`], carrying every field needed
/// for binary/test target selection and `MemberUnitIndex` resolution without allocating `unit_cfg`,
/// `deps`, `meta`, `left_out`, or per-unit strings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScannedUnit<'b> {
    pub index: usize,
    pub name: &'b str,
    pub version: &'b str,
    pub kind: ScannedKind<'b>,
    pub for_host: bool,
    pub edition: PackageEdition,
    pub crate_name: &'b str,
    pub root: &'b str,
    pub has_test_cfg: bool,
    pub has_build_script: bool,
}

/// Zero-copy, zero-excess-alloc scan of a `BUG4` unit-graph record: validates the entire record
/// (magic, version, platforms, unit indices, UTF-8, metadata order, left-out order, trailing
/// bytes) identically to [`decode`], while borrowing every string directly from `bytes` and
/// skipping `unit_cfg`, `deps`, `meta`, and `left_out` allocations.
pub fn scan_units<'b, 'a>(bytes: &'b [u8], alloc: Alloc<'a>) -> Result<AVec<'a, ScannedUnit<'b>>, RecordRefused> {
    let mut r = Reader { bytes, at: 0, alloc };
    if r.take(4)? != MAGIC {
        return Err(RecordRefused { at: 0, why: "another magic (an older form, or not a record)" });
    }
    match r.u8()? {
        1..=3 => {}
        _ => return r.refuse("no such resolver"),
    }
    let target_has_test = r.skip_platform()?;
    let host_has_test = r.skip_platform()?;
    let n = r.count()?;
    let mut units = Vec::with_capacity_in(n, alloc);
    let mut req_scratch: AVec<'a, &'b str> = Vec::new_in(alloc);
    let mut feat_scratch: AVec<'a, &'b str> = Vec::new_in(alloc);
    for index in 0..n {
        let _package = r.u32()?;
        let name = r.str_ref()?;
        let version = r.str_ref()?;
        req_scratch.clear();
        let kind_tag = r.u8()?;
        let mut extra_info: Option<(u8, &'b str, bool, bool)> = None;
        let simple_kind = match kind_tag {
            0 => Some(ScannedKind::Lib),
            1 => Some(ScannedKind::ProcMacro),
            2 => Some(ScannedKind::Bin(r.str_ref()?)),
            3 => Some(ScannedKind::BuildScript),
            4 => Some(ScannedKind::Test { bin: None }),
            5 => Some(ScannedKind::Test { bin: Some(r.str_ref()?) }),
            tag @ 6..=8 => {
                let target_name = r.str_ref()?;
                let flags = r.u8()?;
                if flags > 3 {
                    return r.refuse("unknown target flags");
                }
                let nr = r.count()?;
                req_scratch.reserve(nr);
                for _ in 0..nr {
                    req_scratch.push(r.str_ref()?);
                }
                extra_info = Some((tag, target_name, flags & 1 != 0, flags & 2 != 0));
                None
            }
            _ => return r.refuse("no such unit kind"),
        };
        let flags = r.u8()?;
        if flags > 1 {
            return r.refuse("unknown unit flags");
        }
        let for_host = flags & 1 != 0;
        let edition = r.edition_ref()?;
        let crate_name = r.str_ref()?;
        let root = r.str_ref()?;
        let nf = r.count()?;
        let required_features_met = if req_scratch.is_empty() {
            for _ in 0..nf {
                let _ = r.str_ref()?;
            }
            true
        } else {
            feat_scratch.clear();
            feat_scratch.reserve(nf);
            for _ in 0..nf {
                feat_scratch.push(r.str_ref()?);
            }
            req_scratch.iter().all(|f| feat_scratch.binary_search(f).is_ok())
        };
        let kind = match (simple_kind, extra_info) {
            (Some(k), _) => k,
            (None, Some((6, target_name, harness, test))) => ScannedKind::IntegrationTest { name: target_name, harness, test, required_features_met },
            (None, Some((7, target_name, harness, test))) => ScannedKind::Example { name: target_name, harness, test, required_features_met },
            (None, Some((_, target_name, harness, test))) => ScannedKind::Bench { name: target_name, harness, test, required_features_met },
            (None, None) => return r.refuse("no such unit kind"),
        };
        let has_test_cfg = (if for_host { host_has_test } else { target_has_test }) || kind.has_test_cfg();
        let nd = r.count()?;
        for _ in 0..nd {
            let _ = r.str_ref()?;
            let _ = r.unit(n)?;
        }
        let has_build_script = match r.u32()? {
            u32::MAX => false,
            id if usize::try_from(id).is_ok_and(|u| u < n) => true,
            _ => return r.refuse("a build script past the last unit"),
        };
        match r.u8()? {
            0 => {}
            1 => {
                let _ = r.str_ref()?;
            }
            _ => return r.refuse("a links tag"),
        }
        units.push(ScannedUnit { index, name, version, kind, for_host, edition, crate_name, root, has_test_cfg, has_build_script });
    }
    let nm = r.count()?;
    let mut last_meta: Option<LockId> = None;
    for _ in 0..nm {
        let id = LockId(r.u32()?);
        if last_meta.is_some_and(|last| last >= id) {
            return r.refuse("package metadata not strictly by id");
        }
        last_meta = Some(id);
        for _ in 0..8 {
            let _ = r.str_ref()?;
        }
    }
    let nl = r.count()?;
    let mut last_left: Option<(LockId, LeftOutKind, &'b str)> = None;
    for _ in 0..nl {
        let package = LockId(r.u32()?);
        let _package_name = r.str_ref()?;
        let kind = match r.u8()? {
            0 => LeftOutKind::Bin,
            1 => LeftOutKind::Test,
            2 => LeftOutKind::Example,
            3 => LeftOutKind::Bench,
            _ => return r.refuse("no such left-out target kind"),
        };
        let name = r.str_ref()?;
        let no = r.count()?;
        if no == 0 {
            return r.refuse("a target left out with no feature off");
        }
        for _ in 0..no {
            let _ = r.str_ref()?;
        }
        if last_left.is_some_and(|last| last >= (package, kind, name)) {
            return r.refuse("left-out targets not strictly by (package, kind, name)");
        }
        last_left = Some((package, kind, name));
    }
    if r.at != bytes.len() {
        return r.refuse("bytes after the record");
    }
    Ok(units)
}

/// The digest of what a request's unit graph read: the record's source.
pub struct Source(Hasher);

impl Default for Source {
    fn default() -> Source {
        Source::new()
    }
}

impl Source {
    pub fn new() -> Source {
        Source(Hasher::new(SOURCE_DOMAIN))
    }

    /// The lock's bytes: they pin each dependency's sources (a crate's SHA-256, a git commit),
    /// so the manifests read from those records are covered by it.
    pub fn lock(&mut self, bytes: &[u8]) -> &mut Source {
        self.0.part(b"lock").part(bytes);
        self
    }

    /// The digest of everything the workspace's layout read, when another task (the driver's
    /// `Units`) read it: in place of [`Source::manifest`] and [`Source::listing`] one by one.
    pub fn workspace(&mut self, digest: &[u8; 32]) -> &mut Source {
        self.0.part(b"workspace").part(digest);
        self
    }

    /// A workspace manifest the layout read (`None`: there was none at `path`).
    pub fn manifest(&mut self, path: &str, text: Option<&str>) -> &mut Source {
        self.0.part(b"manifest").part(path.as_bytes());
        match text {
            Some(text) => self.0.part(b"some").part(text.as_bytes()),
            None => self.0.part(b"none"),
        };
        self
    }

    /// A workspace directory listing the layout read (`None`: no such directory).
    pub fn listing(&mut self, dir: &str, entries: Option<&[Entry<'_>]>) -> &mut Source {
        self.0.part(b"listing").part(dir.as_bytes());
        match entries {
            Some(entries) => {
                self.0.part(&(entries.len() as u64).to_le_bytes());
                for e in entries {
                    self.0.part(e.name.as_bytes()).part(&[u8::from(e.is_dir)]);
                }
            }
            None => {
                self.0.part(b"none");
            }
        }
        self
    }

    /// The target's or the host's description (`role`: `"target"` or `"host"`).
    pub fn platform(&mut self, role: &str, desc: &TargetDesc<'_>) -> &mut Source {
        self.0.part(role.as_bytes()).part(desc.triple.as_bytes()).part(&(desc.cfg.len() as u64).to_le_bytes());
        for atom in desc.cfg.iter() {
            self.0.part(atom.name.as_bytes());
            match &atom.value {
                Some(v) => self.0.part(b"=").part(v.as_bytes()),
                None => self.0.part(b"flag"),
            };
        }
        self
    }

    /// What was asked: the roots (positions in the lock, which the lock's bytes fix), the
    /// features and the flags.
    pub fn ask(&mut self, ask: &Ask<'_>) -> &mut Source {
        self.0.part(b"ask").part(&(ask.roots.len() as u64).to_le_bytes());
        for root in ask.roots.iter() {
            self.0.part(&root.0.to_le_bytes());
        }
        self.0.part(&(ask.features.len() as u64).to_le_bytes());
        for f in ask.features.iter() {
            self.0.part(f.as_bytes());
        }
        self.0.part(&[u8::from(ask.all_features), u8::from(ask.no_default_features), u8::from(ask.tests)]);
        self
    }

    /// The toolchain's id, when the sysroot's units are joined.
    pub fn toolchain(&mut self, id: &[u8; 32]) -> &mut Source {
        self.0.part(b"toolchain").part(id);
        self
    }

    pub fn finish(&self) -> ContentId {
        self.0.finish()
    }
}

struct Writer<'a>(AVec<'a, u8>);

impl Writer<'_> {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn text(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }

    fn atom(&mut self, atom: &CfgAtom<'_>) {
        self.text(&atom.name);
        match &atom.value {
            None => self.u8(0),
            Some(value) => {
                self.u8(1);
                self.text(value);
            }
        }
    }
}

struct Reader<'b, 'a> {
    bytes: &'b [u8],
    at: usize,
    alloc: Alloc<'a>,
}

impl<'b, 'a> Reader<'b, 'a> {
    fn refuse<T>(&self, why: &'static str) -> Result<T, RecordRefused> {
        Err(RecordRefused { at: self.at, why })
    }

    fn take(&mut self, n: usize) -> Result<&'b [u8], RecordRefused> {
        match self.bytes.get(self.at..self.at.saturating_add(n)) {
            Some(s) => {
                self.at += n;
                Ok(s)
            }
            None => self.refuse("cut short"),
        }
    }

    fn u8(&mut self) -> Result<u8, RecordRefused> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, RecordRefused> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, RecordRefused> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A count, held to what the bytes left could hold (each element is at least 4 bytes), so
    /// no reservation is made from a number the bytes cannot back.
    fn count(&mut self) -> Result<usize, RecordRefused> {
        let n = self.u32()? as usize;
        match n <= (self.bytes.len() - self.at) / 4 {
            true => Ok(n),
            false => self.refuse("a count past the record's end"),
        }
    }

    fn str_ref(&mut self) -> Result<&'b str, RecordRefused> {
        let n = usize::try_from(self.u32()?).map_err(|_| RecordRefused { at: self.at, why: "a string length past usize" })?;
        let at = self.at;
        let bytes = self.take(n)?;
        match core::str::from_utf8(bytes) {
            Ok(s) => Ok(s),
            Err(_) => Err(RecordRefused { at, why: "text that is not UTF-8" }),
        }
    }

    fn text(&mut self) -> Result<Text<'a>, RecordRefused> {
        let s = self.str_ref()?;
        Ok(String::from_str_in(s, self.alloc))
    }

    /// Validates a platform section without allocating strings, returning whether the platform's
    /// cfg atoms already include bare `test`.
    fn skip_platform(&mut self) -> Result<bool, RecordRefused> {
        let _triple = self.str_ref()?;
        let n = self.count()?;
        let mut last: Option<(&'b str, Option<&'b str>)> = None;
        let mut has_test = false;
        for _ in 0..n {
            let name = self.str_ref()?;
            let value = match self.u8()? {
                0 => None,
                1 => Some(self.str_ref()?),
                _ => return self.refuse("a cfg atom's value tag"),
            };
            if last.is_some_and(|prev| prev >= (name, value)) {
                return self.refuse("a platform's atoms out of order");
            }
            last = Some((name, value));
            if name == "test" && value.is_none() {
                has_test = true;
            }
        }
        Ok(has_test)
    }

    fn edition_ref(&mut self) -> Result<PackageEdition, RecordRefused> {
        let at = self.at;
        match self.u8()? {
            0 => {
                let syntax = match self.u16()? {
                    2015 => Edition::Edition2015,
                    2018 => Edition::Edition2018,
                    2021 => Edition::Edition2021,
                    2024 => Edition::Edition2024,
                    _ => return Err(RecordRefused { at, why: "no such Rust edition" }),
                };
                Ok(PackageEdition { syntax, blaze: None })
            }
            1 => {
                let written = self.str_ref()?;
                match PackageEdition::of_manifest(written) {
                    Ok(edition) if edition.blaze.is_some() => Ok(edition),
                    _ => Err(RecordRefused { at, why: "no such blaze edition" }),
                }
            }
            _ => Err(RecordRefused { at, why: "an edition's tag" }),
        }
    }

    /// A platform: its triple and its atoms, which must be sorted and distinct.
    fn platform(&mut self) -> Result<TargetDesc<'a>, RecordRefused> {
        let triple = self.text()?;
        let n = self.count()?;
        let mut cfg: AVec<'a, CfgAtom<'a>> = Vec::with_capacity_in(n, self.alloc);
        for _ in 0..n {
            let name = self.text()?;
            let value = match self.u8()? {
                0 => None,
                1 => Some(self.text()?),
                _ => return self.refuse("a cfg atom's value tag"),
            };
            let atom = CfgAtom { name, value };
            if cfg.last().is_some_and(|last| *last >= atom) {
                return self.refuse("a platform's atoms out of order");
            }
            cfg.push(atom);
        }
        Ok(TargetDesc { triple, cfg })
    }

    fn unit(&mut self, n: usize) -> Result<UnitId, RecordRefused> {
        let id = self.u32()?;
        match (id as usize) < n {
            true => Ok(UnitId(id)),
            false => self.refuse("a dependency past the last unit"),
        }
    }

    /// `0` and a Rust edition's year, or `1` and a blaze edition's manifest spelling.
    fn edition(&mut self) -> Result<PackageEdition, RecordRefused> {
        let at = self.at;
        match self.u8()? {
            0 => {
                let syntax = match self.u16()? {
                    2015 => Edition::Edition2015,
                    2018 => Edition::Edition2018,
                    2021 => Edition::Edition2021,
                    2024 => Edition::Edition2024,
                    _ => return Err(RecordRefused { at, why: "no such Rust edition" }),
                };
                Ok(PackageEdition { syntax, blaze: None })
            }
            1 => {
                let written = self.text()?;
                match PackageEdition::of_manifest(&written) {
                    Ok(edition) if edition.blaze.is_some() => Ok(edition),
                    _ => Err(RecordRefused { at, why: "no such blaze edition" }),
                }
            }
            _ => Err(RecordRefused { at, why: "an edition's tag" }),
        }
    }
}

#[cfg(test)]
mod scan_tests {
    use blaze_manifest::lock::LockId;
    use blaze_own::{String, Vec};
    use edition::{Edition, PackageEdition};

    use super::{ScannedKind, decode, encode, scan_units};
    use crate::resolve::unit_cfg;
    use crate::{CfgAtom, ExtraUnit, Resolver, TargetDesc, Unit, UnitDep, UnitGraph, UnitId, UnitKind};

    #[test]
    fn scan_units_matches_decode_and_refuses_corrupt_records() {
        let alloc = blaze_own::testing::heap().alloc();
        let s = |v: &str| String::from_str_in(v, alloc);
        let target = TargetDesc {
            triple: s("aarch64-apple-darwin"),
            cfg: {
                let mut c = Vec::new_in(alloc);
                c.push(CfgAtom { name: s("target_arch"), value: Some(s("aarch64")) });
                c.push(CfgAtom { name: s("unix"), value: None });
                c
            },
        };
        let host = TargetDesc { triple: s("aarch64-apple-darwin"), cfg: Vec::new_in(alloc) };
        let ed = PackageEdition { syntax: Edition::Edition2024, blaze: None };

        let make_unit = |kind, name: &str, crate_name: &str, root: &str, for_host: bool, feats: &[&str], build_script: Option<UnitId>| {
            let mut features = Vec::new_in(alloc);
            for &f in feats {
                features.push(s(f));
            }
            let desc = if for_host { &host } else { &target };
            let cfg = unit_cfg(
                desc,
                &features,
                crate::extra::has_test_cfg(&kind),
                crate::resolve::ignores_profile_panic(&kind),
                kind == UnitKind::ProcMacro,
                alloc,
            );
            Unit {
                package: LockId(0),
                name: s(name),
                version: s("0.1.0"),
                kind,
                for_host,
                edition: ed,
                crate_name: s(crate_name),
                root: s(root),
                features,
                cfg,
                deps: Vec::new_in(alloc),
                build_script,
                links: None,
            }
        };

        let mut units = Vec::new_in(alloc);
        units.push(make_unit(UnitKind::BuildScript, "app", "build_script_build", "build.rs", true, &[], None));
        units.push(make_unit(UnitKind::Lib, "app", "app", "src/lib.rs", false, &["alpha"], Some(UnitId(0))));
        units.push(make_unit(UnitKind::Bin(s("cli")), "app", "cli", "src/main.rs", false, &["alpha"], Some(UnitId(0))));
        units.push(make_unit(UnitKind::Test { bin: None }, "app", "app", "src/lib.rs", false, &["alpha"], Some(UnitId(0))));
        units.push(make_unit(
            UnitKind::IntegrationTest(ExtraUnit {
                name: s("it_ok"),
                harness: true,
                test: true,
                required_features: {
                    let mut r = Vec::new_in(alloc);
                    r.push(s("alpha"));
                    r
                },
            }),
            "app",
            "it_ok",
            "tests/it_ok.rs",
            false,
            &["alpha", "beta"],
            None,
        ));
        units.push(make_unit(
            UnitKind::IntegrationTest(ExtraUnit {
                name: s("it_missing"),
                harness: false,
                test: true,
                required_features: {
                    let mut r = Vec::new_in(alloc);
                    r.push(s("gamma"));
                    r
                },
            }),
            "app",
            "it_missing",
            "tests/it_missing.rs",
            false,
            &["alpha"],
            None,
        ));
        // Add a dependency edge on unit 1 -> unit 0 to exercise dep skipping.
        units[1].deps.push(UnitDep { name: s("bs"), unit: UnitId(0) });

        let graph = UnitGraph { units, resolver: Resolver::V3, meta: Vec::new_in(alloc), left_out: Vec::new_in(alloc) };
        let encoded = encode(&graph, &target, &host, alloc).expect("encode unit graph");
        let full = decode(encoded.as_slice(), alloc).expect("full decode");
        let scanned = scan_units(encoded.as_slice(), alloc).expect("scan_units");

        assert_eq!(scanned.len(), full.graph.units.len());
        for (sc, u) in scanned.iter().zip(full.graph.units.iter()) {
            assert_eq!(sc.name, u.name.as_str());
            assert_eq!(sc.version, u.version.as_str());
            assert_eq!(sc.crate_name, u.crate_name.as_str());
            assert_eq!(sc.root, u.root.as_str());
            assert_eq!(sc.for_host, u.for_host);
            assert_eq!(sc.edition, u.edition);
            assert_eq!(sc.has_build_script, u.build_script.is_some());
            assert_eq!(sc.has_test_cfg, u.cfg.iter().any(|c| c.name == "test" && c.value.is_none()));
        }
        assert_eq!(
            scanned[4].kind,
            ScannedKind::IntegrationTest { name: "it_ok", harness: true, test: true, required_features_met: true }
        );
        assert_eq!(
            scanned[5].kind,
            ScannedKind::IntegrationTest { name: "it_missing", harness: false, test: true, required_features_met: false }
        );

        // Corrupt magic and truncated payload must be refused identically.
        let mut bad_magic = Vec::new_in(alloc);
        bad_magic.extend_from_slice(encoded.as_slice());
        bad_magic[0] = b'X';
        assert!(scan_units(bad_magic.as_slice(), alloc).is_err());
        assert!(scan_units(&encoded[..encoded.len() - 1], alloc).is_err());
    }
}
