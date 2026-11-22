//! Select a member's direct dependencies by Cargo key before looking up package names.

use std::path::Path;

use blaze_own::{ASortedMap, ASortedSet, AString, AVec, Alloc, CollectIn};
use blaze_toml::{Document, Edit, Item, Limits, Origin};

use super::Failure;

type Node<'d, 't, 'a> = Item<'d, 't, Alloc<'a>>;

pub(super) fn parse<'t, 'a>(text: &'t str, path: &Path, alloc: Alloc<'a>) -> Result<Document<'t, Alloc<'a>>, Failure> {
    let doc = Document::parse_in(text, Limits::CARGO, alloc).map_err(|why| Failure::Manifest { path: path.to_owned(), why: why.display(text).to_string() })?;
    if let Some(features) = doc.root().get("features") {
        for feature in features.children() {
            if !feature.is_array() || feature.children().any(|entry| entry.as_str().is_none()) {
                return Err(Failure::Manifest { path: path.to_owned(), why: ["expected an array of strings in feature ", feature.key()].concat() });
            }
        }
    }
    Ok(doc)
}

fn optional(dep: Node<'_, '_, '_>) -> bool {
    dep.get("optional").and_then(|value| value.as_bool()) == Some(true)
}

/// The dependency tables an optional dependency can be declared in (dev-dependencies cannot be
/// optional).
pub(super) const ALL_KINDS: [&str; 2] = ["dependencies", "build-dependencies"];

/// The implicit features of a manifest: THE rule every manifest this builder writes keeps its
/// package's features by. The Cargo Book, "Features", "Optional dependencies": an optional
/// dependency "implicitly defines a feature" of its name, and "If you specify the optional
/// dependency with the `dep:` prefix anywhere in the `[features]` table, that disables the
/// implicit feature"; a feature the table declares under that name is that name's feature. So:
/// each optional key of the dependency tables `kinds` names (`dependencies`,
/// `build-dependencies`; their `[target.*]` forms too) that no `dep:` entry names and the table
/// does not declare. A written manifest keeps
/// each: implicit while its dependency stays (declared, `"cc" = []` hides the dependency and
/// cargo refuses the manifest: "optional dependency `cc` is not included in any feature"), and
/// declared empty once the dependency is removed (dropped, a dependent's `phf_shared/unicase`
/// names a feature that is not there, and cargo refuses the resolve).
pub(super) fn implicit_features<'d, 't, 'a>(doc: &'d Document<'t, Alloc<'a>>, kinds: &[&str], alloc: Alloc<'a>) -> ASortedSet<'a, &'d str> {
    let root = doc.root();
    let features = root.get("features");
    let mut hidden = ASortedSet::new_in(alloc);
    for feature in features.into_iter().flat_map(|table| table.children()) {
        for entry in feature.children().filter_map(|value| value.as_str()) {
            if let Some(key) = entry.strip_prefix("dep:") {
                hidden.insert(key);
            }
        }
    }
    let mut out = ASortedSet::new_in(alloc);
    let targets = root.get("target").into_iter().flat_map(|table| table.children());
    for scope in std::iter::once(root).chain(targets) {
        for table in kinds.iter().filter_map(|kind| scope.get(kind)) {
            for dep in table.children() {
                let declared = features.and_then(|table| table.get(dep.key())).is_some();
                if optional(dep) && !hidden.contains(dep.key()) && !declared {
                    out.insert(dep.key());
                }
            }
        }
    }
    out
}

/// A declaration's package name in the lock: `package = ".."` where renamed, else its key.
fn package_of<'d>(dep: Node<'d, '_, '_>) -> &'d str {
    dep.get("package").and_then(|value| value.as_str()).unwrap_or_else(|| dep.key())
}

/// What a package's enabled features turn on, by Cargo's feature resolution: the closure, in
/// the package's own `[features]` table, of the features its dependents request of it.
struct Closure<'d, 'a> {
    /// Each optional dependency key turned on, with the feature that turned it on (a requested
    /// feature, or the feature whose list held the entry).
    enabled: ASortedMap<'a, &'d str, &'d str>,
    /// Each feature forwarded to a dependency: its key, the feature, and whether the entry was
    /// weak (`key?/feat`: forwarded only if something else turns the dependency on).
    forwards: AVec<'a, (&'d str, &'d str, bool)>,
}

/// Turns `key` on, keeping the first feature that did.
fn enable<'k>(enabled: &mut ASortedMap<'_, &'k str, &'k str>, key: &'k str, via: &'k str) {
    if enabled.get(key).is_none() {
        enabled.insert(key, via);
    }
}

/// A strong dependency feature or an implicit feature enables its dependency. A weak
/// dependency feature only forwards a feature if something else enabled that dependency.
fn closure<'d, 'a, R: AsRef<str>>(
    features: Option<Node<'d, '_, 'a>>,
    requested: &'d [R],
    optional_keys: &ASortedSet<'a, &str>,
    alloc: Alloc<'a>,
) -> Closure<'d, 'a> {
    let mut hidden = ASortedSet::new_in(alloc);
    for feature in features.into_iter().flat_map(|table| table.children()) {
        for entry in feature.children().filter_map(|value| value.as_str()) {
            if let Some(key) = entry.strip_prefix("dep:") {
                hidden.insert(key);
            }
        }
    }
    // Each entry with the feature it was reached from.
    let mut pending: AVec<'a, (&'d str, &'d str)> = requested.iter().map(|name| (name.as_ref(), name.as_ref())).collect_in(alloc);
    let mut visited = ASortedSet::new_in(alloc);
    let mut out = Closure { enabled: ASortedMap::new_in(alloc), forwards: AVec::new_in(alloc) };
    while let Some((entry, via)) = pending.pop() {
        if !visited.insert(entry) {
            continue;
        }
        if let Some(key) = entry.strip_prefix("dep:") {
            enable(&mut out.enabled, key, via);
        } else if let Some((key, forwarded)) = entry.split_once('/') {
            let (key, weak) = match key.strip_suffix('?') {
                Some(key) => (key, true),
                None => (key, false),
            };
            out.forwards.push((key, forwarded, weak));
            if !weak && optional_keys.contains(key) {
                enable(&mut out.enabled, key, via);
                // Strong forwarding to an optional dependency also activates a declared
                // local feature of the same name. A weak entry cannot.
                if features.and_then(|table| table.get(key)).is_some() {
                    pending.push((key, via));
                }
            }
        } else if let Some(feature) = features.and_then(|table| table.get(entry)) {
            pending.extend(feature.children().filter_map(|value| value.as_str()).map(|listed| (listed, entry)));
        } else if !hidden.contains(entry) {
            enable(&mut out.enabled, entry, via);
        }
    }
    out
}

/// One dependency edge in the build: a kept declaration and what it requests of its package.
pub(super) struct Edge<'d, 'a> {
    /// The package's name in the lock (`package = ".."` where renamed, else the key).
    pub(super) package: &'d str,
    /// The features requested of the package by this declaration: its `features`, `default`
    /// unless `default-features = false`, and every feature the enabled closure forwards to
    /// its key (weak entries too: the dependency is on).
    pub(super) features: AVec<'a, &'d str>,
    /// For an optional dependency, the feature that turned it on; `None` for a required one.
    pub(super) enabled_by: Option<&'d str>,
}

pub(super) struct MemberDependencies<'d, 't, 'a> {
    kept: AVec<'a, Node<'d, 't, 'a>>,
    removed: AVec<'a, Node<'d, 't, 'a>>,
    removed_keys: AVec<'a, &'d str>,
    /// Optional declarations removed because no enabled feature turns them on.
    removed_optional: usize,
    closure: Closure<'d, 'a>,
}

impl<'d, 't, 'a> MemberDependencies<'d, 't, 'a> {
    /// Both an inline entry and a normalized section are one child of the dependency table.
    /// Optionality belongs to that declaration, even if another target requires the same key.
    /// THE decision for every package of a build, member or vendored dependency: a dependency
    /// edge is in the build iff Cargo's feature resolution enables it from `requested`.
    pub(super) fn select<R: AsRef<str>>(doc: &'d Document<'t, Alloc<'a>>, requested: &'d [R], alloc: Alloc<'a>) -> Self {
        let root = doc.root();
        let mut optional_keys = ASortedSet::new_in(alloc);
        let mut selection = Self {
            kept: AVec::new_in(alloc),
            removed: AVec::new_in(alloc),
            removed_keys: AVec::new_in(alloc),
            removed_optional: 0,
            closure: Closure { enabled: ASortedMap::new_in(alloc), forwards: AVec::new_in(alloc) },
        };
        let targets = root.get("target").into_iter().flat_map(|table| table.children());
        for scope in std::iter::once(root).chain(targets) {
            if let Some(dev) = scope.get("dev-dependencies") {
                selection.removed_keys.extend(dev.children().map(|dep| dep.key()));
                selection.removed.push(dev);
            }
            for table in ["dependencies", "build-dependencies"].into_iter().filter_map(|kind| scope.get(kind)) {
                for dep in table.children() {
                    if optional(dep) {
                        optional_keys.insert(dep.key());
                    }
                    selection.kept.push(dep);
                }
            }
        }
        selection.closure = closure(root.get("features"), requested, &optional_keys, alloc);
        let enabled = &selection.closure.enabled;
        let (removed, removed_keys, removed_optional) = (&mut selection.removed, &mut selection.removed_keys, &mut selection.removed_optional);
        selection.kept.retain(|&dep| {
            if optional(dep) && enabled.get(dep.key()).is_none() {
                removed_keys.push(dep.key());
                removed.push(dep);
                *removed_optional += 1;
                return false;
            }
            true
        });
        selection
    }

    pub(super) fn packages(&self) -> impl Iterator<Item = &'d str> + '_ {
        self.kept.iter().map(|dep| package_of(*dep))
    }

    /// Every kept declaration as an edge of the build, with what it requests of its package.
    pub(super) fn edges(&self, alloc: Alloc<'a>) -> AVec<'a, Edge<'d, 'a>> {
        let mut out = AVec::with_capacity_in(self.kept.len(), alloc);
        for &dep in &self.kept {
            let mut features = AVec::new_in(alloc);
            features.extend(dep.get("features").into_iter().flat_map(|list| list.children()).filter_map(|value| value.as_str()));
            let no_default = ["default-features", "default_features"].into_iter().any(|key| dep.get(key).and_then(|value| value.as_bool()) == Some(false));
            if !no_default {
                features.push("default");
            }
            features.extend(self.closure.forwards.iter().filter(|(key, _, _)| *key == dep.key()).map(|&(_, forwarded, _)| forwarded));
            let enabled_by = if optional(dep) { self.closure.enabled.get(dep.key()).copied() } else { None };
            out.push(Edge { package: package_of(dep), features, enabled_by });
        }
        out
    }

    /// Whether the decision removes an optional declaration: a manifest it does not change
    /// that way is written as its package has it.
    pub(super) fn removes_optional(&self) -> bool {
        self.removed_optional != 0
    }

    /// Physical sections extend up to the next header, including trailing comments. Only
    /// TOML header nodes mark boundaries: bracketed text inside a value is still its data.
    fn without_sections(&self, doc: &Document<'t, Alloc<'a>>, alloc: Alloc<'a>) -> Option<AString<'a>> {
        let mut removed_tables = ASortedSet::new_in(alloc);
        for node in &self.removed {
            if !matches!(node.origin(), Origin::Inline | Origin::Value) {
                removed_tables.insert(node.key_span().start);
            }
        }
        if removed_tables.is_empty() {
            return None;
        }
        let mut boundaries = AVec::new_in(alloc);
        let mut pending = AVec::new_in(alloc);
        pending.push((doc.root(), false));
        while let Some((node, inside)) = pending.pop() {
            if matches!(node.origin(), Origin::Inline | Origin::Value) {
                continue;
            }
            let dropped = inside || (node.parent().is_some() && removed_tables.contains(&node.key_span().start));
            if node.is_table() && node.origin() == Origin::Header && node.parent().is_some() {
                let start = doc.text()[..node.span().start].rfind('\n').map_or(0, |at| at + 1);
                boundaries.push((start, dropped));
            }
            pending.extend(node.children().map(|child| (child, dropped)));
        }
        if !boundaries.iter().any(|&(_, dropped)| dropped) {
            return None;
        }
        boundaries.sort_unstable();
        let mut out = AString::with_capacity_in(doc.text().len(), alloc);
        let mut start = 0;
        let mut keep = true;
        for (end, dropped) in boundaries {
            if keep {
                out.push_str(&doc.text()[start..end]);
            }
            start = end;
            keep = !dropped;
        }
        if keep {
            out.push_str(&doc.text()[start..]);
        }
        Some(out)
    }

    fn keep_entry(&self, features: Node<'_, '_, '_>, entry: &str) -> bool {
        let key = if let Some(key) = entry.strip_prefix("dep:") {
            return !self.removed_keys.contains(&key) || self.kept.iter().any(|dep| dep.key() == key && optional(*dep));
        } else if let Some((dep, _)) = entry.split_once('/') {
            dep.trim_end_matches('?')
        } else if features.get(entry).is_some() {
            return true;
        } else {
            return !self.removed_keys.contains(&entry) || self.kept.iter().any(|dep| dep.key() == entry && optional(*dep));
        };
        !self.removed_keys.contains(&key) || self.kept.iter().any(|dep| dep.key() == key)
    }
}

/// The corresponding key after entire sections have been cut. Dependency paths consist of
/// table keys only (at most target, cfg, kind, name).
fn corresponding<'d, 't, 'a>(source: Node<'_, '_, '_>, root: Node<'d, 't, 'a>) -> Option<Node<'d, 't, 'a>> {
    match source.parent() {
        None => Some(root),
        Some(parent) => corresponding(parent, root)?.get(source.key()),
    }
}

/// Apply the same selection used by the store. TOML node removal covers the complete
/// declaration, including multi-line arrays, inline tables and dotted keys.
pub(super) fn filtered_manifest<'a>(text: &str, path: &Path, requested: &[String], alloc: Alloc<'a>) -> Result<AString<'a>, Failure> {
    let doc = parse(text, path, alloc)?;
    let selected = MemberDependencies::select(&doc, requested, alloc);
    written(&doc, &selected, path, alloc)
}

/// A vendored dependency's manifest under the same decision as a member's: `None` when no
/// optional declaration falls outside the closure of `requested` (its features as the build's
/// edges request them), so the package's own bytes stay; else the manifest without those
/// declarations and the feature entries naming them. Its dev-dependencies go with them: Cargo
/// never resolves a dependency's.
pub(super) fn rewritten_manifest<'a, R: AsRef<str>>(text: &str, path: &Path, requested: &[R], alloc: Alloc<'a>) -> Result<Option<AString<'a>>, Failure> {
    let doc = parse(text, path, alloc)?;
    let selected = MemberDependencies::select(&doc, requested, alloc);
    if !selected.removes_optional() {
        return Ok(None);
    }
    written(&doc, &selected, path, alloc).map(Some)
}

fn written<'d, 't, 'a>(doc: &'d Document<'t, Alloc<'a>>, selected: &MemberDependencies<'d, 't, 'a>, path: &Path, alloc: Alloc<'a>) -> Result<AString<'a>, Failure> {
    let cut = selected.without_sections(doc, alloc);
    let remaining = cut.as_ref().map(|text| parse(text.as_str(), path, alloc)).transpose()?;
    let base = remaining.as_ref().unwrap_or(doc);
    let mut edit = Edit::new_in(base, alloc);
    let refuse = |why: blaze_toml::EditError<Alloc<'a>>| Failure::Manifest { path: path.to_owned(), why: why.to_string() };
    for &dep in &selected.removed {
        if let Some(declaration) = corresponding(dep, base.root()) {
            edit.remove(declaration).map_err(refuse)?;
        }
    }
    let features = doc.root().get("features");
    if let Some(table) = base.root().get("features") {
        edit.remove(table).map_err(refuse)?;
    }
    let mut out = edit.finish().map_err(refuse)?;
    // The package's features as its own manifest has them ([`implicit_features`]): the implicit
    // feature of an optional dependency the decision removed is written declared and empty, so
    // what forwards to it still resolves (cargo resolves a member's lock with every feature on:
    // phf_macros' `unicase = ["phf_shared/unicase"]` against phf_shared with `unicase` cut); one
    // whose dependency stays stays implicit.
    let mut implicit = implicit_features(doc, &ALL_KINDS, alloc);
    implicit.retain(|key| !selected.kept.iter().any(|dep| dep.key() == *key));
    if features.is_some() || !implicit.is_empty() {
        out.push_str("\n[features]\n");
    }
    for &key in implicit.iter() {
        blaze_toml::write_string(key, &mut out);
        out.push_str(" = []\n");
    }
    if let Some(table) = features {
        for feature in table.children() {
            blaze_toml::write_string(feature.key(), &mut out);
            out.push_str(" = [");
            let mut separator = "";
            for value in feature.children() {
                let entry = value.as_str().expect("parse checked the feature's entries");
                if selected.keep_entry(table, entry) {
                    out.push_str(separator);
                    if let Some((key, forwarded)) = entry.split_once("?/")
                        && selected.removed_keys.contains(&key)
                        && !selected.kept.iter().any(|dep| dep.key() == key && optional(*dep))
                    {
                        // The remaining declarations are required, so forwarding is unconditional.
                        let mut strong = AString::new_in(alloc);
                        strong.push_str(key);
                        strong.push('/');
                        strong.push_str(forwarded);
                        blaze_toml::write_string(strong.as_str(), &mut out);
                    } else {
                        blaze_toml::write_string(entry, &mut out);
                    }
                    separator = ", ";
                }
            }
            out.push_str("]\n");
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dependency U (not the member) with an optional dependency in the inline form that only
    /// its feature `include-exclude` turns on.
    const U: &str = "[package]\nname = \"u\"\nversion = \"0.1.0\"\n\n[dependencies]\nglobset = { version = \"0.4\", optional = true }\nsha2 = \"0.11\"\n\n[features]\ninclude-exclude = [\"globset\"]\n";

    /// Cargo's feature resolution: with no feature of U requested (`default` only, which U
    /// does not declare), no enabled feature turns `globset` on, so it is not in the build.
    /// Prints "u without globset".
    #[test]
    fn an_optional_dependency_no_enabled_feature_turns_on_is_removed() {
        let alloc = blaze_own::testing::heap().alloc();
        let requested = ["default"];
        let written = rewritten_manifest(U, Path::new("u/Cargo.toml"), &requested, alloc).unwrap_or_else(|failure| panic!("{failure}"));
        let written = written.unwrap_or_else(|| panic!("globset was kept"));
        let doc = parse(written.as_str(), Path::new("u/Cargo.toml"), alloc).unwrap_or_else(|failure| panic!("{failure}"));
        let dependencies = doc.root().get("dependencies").unwrap_or_else(|| panic!("no dependencies: {}", written.as_str()));
        assert!(dependencies.get("globset").is_none(), "{}", written.as_str());
        assert!(dependencies.get("sha2").is_some(), "{}", written.as_str());
        let feature = doc.root().get("features").and_then(|table| table.get("include-exclude")).unwrap_or_else(|| panic!("the feature is gone"));
        assert_eq!(feature.children().count(), 0, "{}", written.as_str());
        println!("u without globset");
    }

    /// Requesting `include-exclude` turns `globset` on (a bare entry naming an optional
    /// dependency is its implicit feature), so the manifest stays as written. Prints "u keeps
    /// globset".
    #[test]
    fn an_optional_dependency_a_requested_feature_turns_on_stays() {
        let alloc = blaze_own::testing::heap().alloc();
        let requested = ["include-exclude"];
        let written = rewritten_manifest(U, Path::new("u/Cargo.toml"), &requested, alloc).unwrap_or_else(|failure| panic!("{failure}"));
        assert!(written.is_none(), "{:?}", written.as_ref().map(|text| text.as_str()));
        let doc = parse(U, Path::new("u/Cargo.toml"), alloc).unwrap_or_else(|failure| panic!("{failure}"));
        let selected = MemberDependencies::select(&doc, &requested, alloc);
        let edges = selected.edges(alloc);
        let globset = edges.iter().find(|edge| edge.package == "globset").unwrap_or_else(|| panic!("globset is not an edge"));
        assert_eq!(globset.enabled_by, Some("include-exclude"));
        assert_eq!(globset.features.as_slice(), ["default"]);
        println!("u keeps globset");
    }

    /// phf_shared 0.11.3 as its record has it: `unicase` and `uncased` optional, neither named by
    /// a `dep:` entry, so each defines an implicit feature (its Cargo.toml.orig).
    const PHF_SHARED: &str = "[package]\nname = \"phf_shared\"\nversion = \"0.11.3\"\nedition = \"2021\"\n\n[dependencies.siphasher]\nversion = \"1.0\"\n\n[dependencies.uncased]\nversion = \"0.9.9\"\noptional = true\ndefault-features = false\n\n[dependencies.unicase]\nversion = \"2.4.0\"\noptional = true\n\n[features]\ndefault = [\"std\"]\nstd = []\n";

    /// turso's phf_macros 0.11.3 (2026-11-21): phf_macros requests nothing of phf_shared
    /// (`default-features = false`), so the vendored phf_shared lost `unicase`, and with it its
    /// implicit feature, while phf_macros keeps `unicase = ["phf_shared/unicase"]`: cargo refused
    /// ("depends on `phf_shared` with feature `unicase` but `phf_shared` does not have that
    /// feature"). The written manifest keeps the package's features: `unicase` and `uncased`
    /// declared empty, the dependencies gone. Twin: requesting `unicase` keeps the dependency and
    /// the bytes (both implicit features requested, nothing removed). Control: a package whose
    /// `dep:unicase` entry disables the implicit feature gets no `unicase` feature when the
    /// dependency goes. Prints "phf_shared: ..".
    #[test]
    fn a_removed_optional_dependency_keeps_its_implicit_feature() {
        let alloc = blaze_own::testing::heap().alloc();
        let path = Path::new("phf_shared-0.11.3/Cargo.toml");
        let none: [&str; 0] = [];
        let written = rewritten_manifest(PHF_SHARED, path, &none, alloc).unwrap_or_else(|failure| panic!("{failure}")).unwrap_or_else(|| panic!("unicase was kept"));
        let doc = parse(written.as_str(), path, alloc).unwrap_or_else(|failure| panic!("{failure}"));
        let deps = doc.root().get("dependencies").unwrap_or_else(|| panic!("no dependencies: {}", written.as_str()));
        assert!(deps.get("unicase").is_none() && deps.get("uncased").is_none(), "{}", written.as_str());
        let features = doc.root().get("features").unwrap_or_else(|| panic!("no features: {}", written.as_str()));
        for name in ["unicase", "uncased", "std", "default"] {
            assert!(features.get(name).is_some(), "{name} is gone: {}", written.as_str());
        }
        assert_eq!(features.get("unicase").map(|it| it.children().count()), Some(0));
        assert!(implicit_features(&doc, &ALL_KINDS, alloc).is_empty(), "{}", written.as_str());

        let both = ["unicase", "uncased"];
        let kept = rewritten_manifest(PHF_SHARED, path, &both, alloc).unwrap_or_else(|failure| panic!("{failure}"));
        assert!(kept.is_none(), "{:?}", kept.as_ref().map(|it| it.as_str()));
        let original = parse(PHF_SHARED, path, alloc).unwrap_or_else(|failure| panic!("{failure}"));
        let implicit: Vec<&str> = implicit_features(&original, &ALL_KINDS, alloc).iter().copied().collect();
        assert_eq!(implicit, ["uncased", "unicase"]);

        let hidden = PHF_SHARED.replace("std = []\n", "std = []\nfold = [\"dep:unicase\"]\n");
        let cut = rewritten_manifest(&hidden, path, &none, alloc).unwrap_or_else(|failure| panic!("{failure}")).unwrap_or_else(|| panic!("unicase was kept"));
        let cut_doc = parse(cut.as_str(), path, alloc).unwrap_or_else(|failure| panic!("{failure}"));
        let cut_features = cut_doc.root().get("features").unwrap_or_else(|| panic!("no features: {}", cut.as_str()));
        assert!(cut_features.get("unicase").is_none(), "{}", cut.as_str());
        assert!(cut_features.get("uncased").is_some(), "{}", cut.as_str());
        println!("phf_shared: written {:?} | requested unicase and uncased: unchanged | dep:unicase: no unicase feature", written.as_str());
    }

    /// What an edge requests of its package: its `features`, `default` unless
    /// `default-features = false`, and what the enabled closure forwards to its key, weak
    /// entries included once the dependency is on. Prints "edges request forwarded features".
    #[test]
    fn an_edge_requests_its_features_and_the_forwarded_ones() {
        let manifest = "[package]\nname = \"p\"\nversion = \"0.1.0\"\n\n[dependencies]\nq = { version = \"1\", default-features = false, features = [\"a\"] }\nr = { version = \"1\", optional = true }\n\n[features]\ndefault = [\"q/b\", \"r?/c\"]\nwith-r = [\"dep:r\", \"r/d\"]\n";
        let alloc = blaze_own::testing::heap().alloc();
        let doc = parse(manifest, Path::new("p/Cargo.toml"), alloc).unwrap_or_else(|failure| panic!("{failure}"));
        let requested = ["default", "with-r"];
        let selected = MemberDependencies::select(&doc, &requested, alloc);
        let edges = selected.edges(alloc);
        let of = |name: &str| {
            let edge = edges.iter().find(|edge| edge.package == name).unwrap_or_else(|| panic!("{name} is not an edge"));
            let mut features: Vec<&str> = edge.features.iter().copied().collect();
            features.sort_unstable();
            (features, edge.enabled_by)
        };
        assert_eq!(of("q"), (vec!["a", "b"], None));
        assert_eq!(of("r"), (vec!["c", "d", "default"], Some("with-r")));
        let only_default = ["default"];
        let selected = MemberDependencies::select(&doc, &only_default, alloc);
        assert!(selected.edges(alloc).iter().all(|edge| edge.package != "r"), "a weak entry turned r on");
        println!("edges request forwarded features");
    }
}
