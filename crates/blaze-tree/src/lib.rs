//! `cargo tree`, made by blaze with no cargo: `src/ops/cargo_tree/` of cargo 1.101.0
//! (cad07cf8e5), on the workspace blaze-metadata resolves and blaze-resolve's feature
//! resolver. It prints what cargo prints, line for line. [`query::Tree`] is the same over the database's load (blaze-units' `load`).
//!
//! The work after the feature resolve is O(packages + edges + output bytes): packages are
//! interned once to dense indexes ([`packages::Table`]), the graph is columns of `u32`s with
//! CSR edges ([`graph`]), `-p`, `--prune`, `-i` and the members are bitsets over packages, and
//! the walk keeps its "seen" and "on the stack" as bitsets over nodes and writes into one
//! `String`.

#![forbid(unsafe_code)]

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

mod format;
mod graph;
mod packages;
pub mod query;

use std::cmp::Ordering;
use std::fmt::Write as _;

use anyhow::{Context as _, Result, bail};
use blaze_hash::HashSet;
use blaze_manifest::Workspace;
use blaze_manifest::config::schema::FeatureUnification;
use blaze_manifest::files::Disk;
use blaze_metadata::Asked;
use blaze_resolve::resolver::{
    CliFeatures, CompileKind, FeatureOpts, FeatureResolver, ForceAllTargets, HasDevUnits,
    PackageTargets, ResolveBehavior, ResolvedFeatures, members_with_features,
};
use blaze_resolve::{Cfg, InternedString, PackageId, PackageIdSpec, Platform, Summary};

use blaze_ids::bits::BitSet as Bits;
pub use blaze_resolve::DepKind;
pub use graph::EdgeKind;
use graph::{BUILD, DEV, Edge, Graph, Kind, Node, NodeId, TAGS, Tag};
use packages::{Reads, Table};

/// What `cargo tree` is asked, as its flags say it.
pub struct Options {
    /// The packages to display the tree for.
    pub packages: Packages,
    /// The platform to filter for.
    pub target: Target,
    /// The dependency kinds to display.
    pub edge_kinds: HashSet<EdgeKind>,
    pub invert: Vec<String>,
    /// The packages to prune from the display of the dependency tree.
    pub pkgs_to_prune: Vec<String>,
    /// The style of prefix for each line.
    pub prefix: Prefix,
    /// Whether duplicates are repeated, rather than marked with `(*)` and their dependencies
    /// left out.
    pub no_dedupe: bool,
    /// Only the packages that are in the graph in more than one version. Implies `invert`.
    pub duplicates: bool,
    /// How each package is displayed.
    pub format: String,
    /// Features in the tree as nodes of their own.
    pub graph_features: bool,
    pub display_depth: DisplayDepth,
    /// Excludes proc-macro dependencies.
    pub no_proc_macro: bool,
    /// Include only public dependencies.
    pub public: bool,
    /// `│├└─` rather than `|`-`.
    pub unicode: bool,
    /// Whether the lines are colored, as they are on a terminal.
    pub color: bool,
}

/// `-p`, `--workspace` and `--exclude`: cargo's `Packages`.
pub enum Packages {
    /// None of them: the default members.
    Default,
    /// `--workspace`, with the `-p`s given too.
    All(Vec<String>),
    /// `--workspace --exclude`.
    OptOut(Vec<String>),
    /// `-p`.
    Packages(Vec<String>),
}

#[derive(PartialEq)]
pub enum Target {
    Host,
    Specific(Vec<String>),
    All,
}

impl Target {
    pub fn from_cli(targets: Vec<String>) -> Target {
        match targets.len() {
            0 => Target::Host,
            1 if targets[0] == "all" => Target::All,
            _ => Target::Specific(targets),
        }
    }
}

#[derive(Clone, Copy)]
pub enum Prefix {
    None,
    Indent,
    Depth,
}

#[derive(Clone, Copy)]
pub enum DisplayDepth {
    MaxDisplayDepth(u32),
    Workspace,
}

/// A platform a package is built for: its triple and its `cfg`, as blaze answers
/// `rustc --print cfg` itself.
pub struct Platformed {
    pub triple: String,
    pub cfg: Vec<Cfg>,
}

/// The host, and the `--target`s asked (none: the host only).
pub struct Kinds {
    pub host: Platformed,
    pub targets: Vec<Platformed>,
}

impl Kinds {
    fn requested(&self) -> Vec<CompileKind> {
        // alloc: the platforms asked, once per `cargo tree`.
        let mut requested = Vec::with_capacity(self.targets.len().max(1));
        match self.targets.is_empty() {
            true => requested.push(CompileKind::Host),
            false => requested.extend(self.targets.iter().map(|it| CompileKind::Target(InternedString::new(&it.triple)))),
        }
        requested
    }

    fn activated(&self, platform: &Platform, kind: CompileKind) -> bool {
        let of = match kind {
            CompileKind::Host => Some(&self.host),
            CompileKind::Target(triple) => self.targets.iter().find(|it| it.triple == *triple),
        };
        of.is_some_and(|it| platform.matches(&it.triple, &it.cfg))
    }

    /// The [`Kind`] of `kind`, a platform of these.
    pub(crate) fn kind(&self, kind: CompileKind) -> Kind {
        match kind {
            CompileKind::Host => Kind::HOST,
            CompileKind::Target(triple) => match self.targets.iter().position(|it| it.triple == *triple) {
                Some(i) => Kind(u16::try_from(i + 1).expect("fewer than 2^16 `--target`s")),
                None => panic!("`{triple}` is not a platform asked"),
            },
        }
    }

    fn platformed(&self, kind: Kind) -> &Platformed {
        match kind.0 {
            0 => &self.host,
            i => &self.targets[i as usize - 1],
        }
    }

    pub(crate) fn activated_kind(&self, platform: &Platform, kind: Kind) -> bool {
        let it = self.platformed(kind);
        platform.matches(&it.triple, &it.cfg)
    }

    /// Cargo's order of the platforms, as `CompileKind` has it: the host, then by triple.
    pub(crate) fn cmp_kinds(&self, a: Kind, b: Kind) -> Ordering {
        match (a.0, b.0) {
            (0, 0) => Ordering::Equal,
            (0, _) => Ordering::Less,
            (_, 0) => Ordering::Greater,
            _ => self.platformed(a).triple.cmp(&self.platformed(b).triple),
        }
    }
}

struct Targets<'a> {
    packages: Reads<'a>,
    kinds: &'a Kinds,
}

impl PackageTargets for Targets<'_> {
    fn has_any_proc_macro(&self, package_id: PackageId) -> bool {
        self.packages.get(package_id).is_some_and(|it| it.any_proc_macro)
    }

    fn has_proc_macro_lib(&self, package_id: PackageId) -> bool {
        self.packages.get(package_id).is_some_and(|it| it.proc_macro_lib)
    }

    fn platform_activated(&self, platform: &Platform, kind: CompileKind) -> bool {
        self.kinds.activated(platform, kind)
    }
}

struct Symbols {
    down: &'static str,
    tee: &'static str,
    ell: &'static str,
    right: &'static str,
}

static UTF8_SYMBOLS: Symbols = Symbols { down: "│", tee: "├", ell: "└", right: "─" };

static ASCII_SYMBOLS: Symbols = Symbols { down: "|", tee: "|", ell: "`", right: "-" };

const RESET: &str = "\x1b[0m";

/// The color of a line to a dependency of each tag: cargo's `style::DEP_*`.
const EDGE_COLORS: [&str; TAGS] = ["\x1b[2m", "\x1b[1m\x1b[34m", "\x1b[1m\x1b[36m", "\x1b[1m\x1b[35m"];

/// About a line of output per package, for the output's first size.
const LINE: usize = 48;

/// The tree `opts` asks of the workspace of `asked`, printed into `out`; what cargo warns of
/// goes to `warn`. `asked.features` and its siblings are the command line's.
pub fn print_tree(
    asked: &Asked<'_>,
    opts: &Options,
    kinds: &Kinds,
    out: &mut String,
    // dyn: warnings, a few per `cargo tree` at most, to the CLI's printer.
    warn: &mut dyn FnMut(&str),
) -> Result<()> {
    let build = blaze_metadata::Build {
        packages: &[],
        workspace: false,
        excludes: &[],
        dev_units: opts.edge_kinds.contains(&EdgeKind::Dep(DepKind::Development)),
    };
    let asked = Asked {
        build: Some(build),
        // clone: the command line's, once per `cargo tree`.
        env: asked.env.clone(),
        platform: None,
        ..*asked
    };
    // alloc: the warnings of selecting the packages, said after the resolve.
    let warned = std::cell::RefCell::new(Vec::new());
    let pick = |ws: &Workspace<'_, Disk>, member_ids: &[PackageId]| {
        package_id_specs(&opts.packages, ws, member_ids, &mut |it| {
            // alloc: a warning, rare.
            warned.borrow_mut().push(it.to_owned());
        })
    };
    let printed = blaze_metadata::resolved(&asked, pick, |resolved| {
        let input = Input {
            resolve: resolved.resolve,
            members: resolved.members,
            member_ids: resolved.member_ids,
            specs: resolved.specs,
            cli_features: resolved.cli_features,
            behavior: resolved.behavior,
            unification: resolved.unification,
            packages: Reads::Metadata(resolved.packages),
        };
        out.reserve(LINE * input.resolve.len());
        print_resolved(&input, opts, kinds, out, &mut |it: &str| warn(it))
    });
    for it in warned.into_inner() {
        warn(&it);
    }
    printed
}

/// A workspace resolved for `cargo tree`: blaze-metadata's (v1) or the database's ([`query`]).
pub(crate) struct Input<'a> {
    pub resolve: &'a blaze_resolve::Resolve,
    pub members: blaze_resolve::resolver::Members<'a>,
    pub member_ids: &'a [PackageId],
    pub specs: &'a [PackageIdSpec],
    pub cli_features: &'a CliFeatures,
    pub behavior: ResolveBehavior,
    pub unification: FeatureUnification,
    pub packages: Reads<'a>,
}

/// The members a feature resolve is of: the specs it is for and the members with their
/// features.
type FeatureMembers<'a> = (Vec<PackageIdSpec>, Vec<(&'a Summary, CliFeatures)>);

/// The feature resolves of `input`, as cargo's `resolve_ws_with_opts` makes them under
/// `resolver.feature-unification`: one of the selected packages, which resolves the features
/// of every member under `workspace`; one per selected package under `package`
/// (blaze-metadata's `Resolved::feature_resolves`).
fn feature_resolves<'a>(input: &Input<'a>) -> Result<Vec<FeatureMembers<'a>>> {
    let all: Vec<PackageIdSpec>;
    let resolved_for = match input.unification {
        FeatureUnification::Workspace => {
            // alloc: every member's spec, once per `cargo tree`.
            all = input.member_ids.iter().map(|id| id.to_spec()).collect();
            &all[..]
        }
        FeatureUnification::Selected | FeatureUnification::Package => input.specs,
    };
    let members = members_with_features(&input.members, resolved_for, input.cli_features)?;
    if !matches!(input.unification, FeatureUnification::Package) {
        // alloc: the one feature resolve, its specs the selected packages'.
        return Ok(vec![(input.specs.to_vec(), members)]);
    }
    let one = |spec: &PackageIdSpec| -> Result<FeatureMembers<'a>> {
        // clone: the command line's features, narrowed to this package's, once per package.
        let mut narrowed = input.cli_features.clone();
        let enabled = members.iter().filter(|(member, _)| spec.matches(member.package_id()));
        // alloc: as `narrowed`.
        narrowed.features = std::rc::Rc::new(enabled.flat_map(|(_, it)| it.features.iter().cloned()).collect());
        // alloc: clone: the one spec this resolve is for.
        let specs = vec![spec.clone()];
        let members = members_with_features(&input.members, &specs, &narrowed)?;
        Ok((specs, members))
    };
    // alloc: one resolve per selected package.
    input.specs.iter().map(one).collect()
}

/// One tree to print: the specs it is of, its features resolved, and its members.
pub(crate) struct Resolved<'a> {
    specs: Vec<PackageIdSpec>,
    features: ResolvedFeatures,
    members: Vec<(&'a Summary, CliFeatures)>,
}

/// The tree `opts` asks of `input`, printed into `out`; what cargo warns of goes to `warn`.
pub(crate) fn print_resolved(
    input: &Input<'_>,
    opts: &Options,
    kinds: &Kinds,
    out: &mut String,
    warn: &mut impl FnMut(&str),
) -> Result<()> {
    let requested_kinds = kinds.requested();
    let targets = Targets { packages: input.packages, kinds };
    // Each tree resolved as it is printed, as cargo does: a later resolve's error comes after
    // an earlier tree's warnings.
    let trees = feature_resolves(input)?
        .into_iter()
        .map(|(specs, members)| resolve_tree(input, opts, &targets, &requested_kinds, specs, members));
    print_trees(input, opts, kinds, trees, out, warn)
}

/// The features of each tree of `input`: one per feature resolve, so one per package under
/// `resolver.feature-unification = "package"`.
#[cfg(test)]
pub(crate) fn resolve_trees<'a>(input: &Input<'a>, opts: &Options, kinds: &Kinds) -> Result<Vec<Resolved<'a>>> {
    let requested_kinds = kinds.requested();
    let targets = Targets { packages: input.packages, kinds };
    feature_resolves(input)?
        .into_iter()
        .map(|(specs, members)| resolve_tree(input, opts, &targets, &requested_kinds, specs, members))
        .collect()
}

/// The features of one tree, resolved for `feature_members`, and the members it prints.
fn resolve_tree<'a>(
    input: &Input<'a>,
    opts: &Options,
    targets: &Targets<'_>,
    requested_kinds: &[CompileKind],
    specs: Vec<PackageIdSpec>,
    feature_members: Vec<(&'a Summary, CliFeatures)>,
) -> Result<Resolved<'a>> {
    let has_dev = match opts.edge_kinds.contains(&EdgeKind::Dep(DepKind::Development)) {
        true => HasDevUnits::Yes,
        false => HasDevUnits::No,
    };
    let force_all = match opts.target == Target::All {
        true => ForceAllTargets::Yes,
        false => ForceAllTargets::No,
    };
    let feature_opts = FeatureOpts::new(input.behavior, has_dev, force_all);
    let features = FeatureResolver::resolve(targets, input.resolve, &feature_members, requested_kinds, feature_opts)?;
    let members = members_with_features(&input.members, &specs, input.cli_features)?;
    Ok(Resolved { specs, features, members })
}

/// The ids of the packages `specs` name, each spec one: cargo's `Resolve::specs_to_ids`.
fn specs_to_ids(packages: &Table<'_>, specs: &[PackageIdSpec]) -> Result<Vec<PackageId>> {
    // alloc: one id per spec given.
    specs.iter().map(|spec| spec.query(packages.ids())).collect()
}

/// Prints `trees`, each its graph built and walked, into `out`. Past the feature resolve
/// this is the whole of `cargo tree`.
pub(crate) fn print_trees<'a>(
    input: &Input<'a>,
    opts: &Options,
    kinds: &Kinds,
    trees: impl IntoIterator<Item = Result<Resolved<'a>>>,
    out: &mut String,
    warn: &mut impl FnMut(&str),
) -> Result<()> {
    let packages = Table::new(input.resolve, input.packages);
    let requested_kinds = kinds.requested();
    let members = packages.of(input.member_ids);
    let mut labels = format::Labels::new(packages.len());
    let symbols = if opts.unicode { &UTF8_SYMBOLS } else { &ASCII_SYMBOLS };
    let mut pattern = None;
    for tree in trees {
        let Resolved { specs, features, members: feature_members } = tree?;
        let mut graph =
            graph::build(input.resolve, &features, feature_members, kinds, &requested_kinds, &packages, opts);

        let root_ids = match opts.invert.is_empty() {
            true => specs_to_ids(&packages, &specs)?,
            false => {
                // alloc: the specs of `--invert`, once per tree.
                let specs: Vec<PackageIdSpec> = opts.invert.iter().map(|p| PackageIdSpec::parse(p)).collect::<Result<_>>()?;
                specs_to_ids(&packages, &specs)?
            }
        };
        let mut root_indexes = graph.package_nodes(&packages.of(&root_ids));
        if opts.duplicates {
            // `-d -p foo` only shows duplicates within foo's subtree.
            graph.restrict(&root_indexes);
            root_indexes = graph.find_duplicates();
        }
        if !opts.invert.is_empty() || opts.duplicates {
            graph.invert();
        }

        let mut prune = Vec::with_capacity(opts.pkgs_to_prune.len());
        for p in &opts.pkgs_to_prune {
            // Said if the spec is not of the resolved graph.
            let spec = PackageIdSpec::parse(p)?;
            spec.query(packages.ids())?;
            prune.push(spec);
        }
        let pruned = packages.matching(&prune);

        if root_indexes.is_empty() {
            warn(
                "nothing to print.\n\n\
                 To find dependencies that require specific target platforms, \
                 try to use option `--target all` first, and then narrow your search scope accordingly.",
            );
            continue;
        }
        if pattern.is_none() {
            pattern = Some(
                format::Pattern::new(&opts.format)
                    // alloc: an error, said once.
                    .with_context(|| format!("tree format `{}` not valid", opts.format))?,
            );
        }
        let Some(format) = &pattern else { unreachable!("the pattern was just made") };
        let mut printer = Printer {
            graph: &graph,
            format,
            labels: &mut labels,
            symbols,
            pruned: &pruned,
            members: &members,
            opts,
            visited: Bits::new(graph.len()),
            on_stack: Bits::new(graph.len()),
            levels: Vec::with_capacity(graph.len() + 1),
        };
        printer.print(&root_indexes, out);
    }
    Ok(())
}

/// The specs of the packages `packages` selects: cargo's `Packages::to_package_id_specs`. Once
/// per `cargo tree`, before the resolve.
fn package_id_specs(
    packages: &Packages,
    ws: &Workspace<'_, Disk>,
    member_ids: &[PackageId],
    warn: &mut impl FnMut(&str),
) -> Result<Vec<PackageIdSpec>> {
    let glob = |name: &str| name.contains(['*', '?', '[', ']']);
    let unread = |names: &[String]| match names.iter().find(|it| glob(it)) {
        Some(pattern) => bail!("blaze does not read package patterns like `{pattern}` yet"),
        None => Ok(()),
    };
    // alloc: the specs selected.
    let every = || member_ids.iter().map(|id| id.to_spec()).collect();
    Ok(match packages {
        Packages::All(_) => every(),
        Packages::OptOut(opt_out) => {
            unread(opt_out)?;
            let ids: Vec<PackageIdSpec> =
                // alloc: the specs of `--exclude`.
                opt_out.iter().map(|it| PackageIdSpec::parse(it)).collect::<Result<_>>()?;
            let excluded = |id: &PackageId| ids.iter().any(|spec| spec.matches(*id));
            let not_found: Vec<String> = ids
                .iter()
                .filter(|spec| !member_ids.iter().any(|id| spec.matches(*id)))
                // alloc: a warning's names.
                .map(|spec| spec.to_string())
                // alloc: as above.
                .collect();
            if !not_found.is_empty() {
                // alloc: a warning.
                warn(&format!(
                    "excluded package(s) `{}` not found in workspace `{}`",
                    not_found.join(", "),
                    ws.root().display()
                ));
            }
            // alloc: the specs selected.
            member_ids.iter().filter(|id| !excluded(id)).map(|id| id.to_spec()).collect()
        }
        Packages::Packages(opt_in) => {
            unread(opt_in)?;
            let mut specs: Vec<(String, PackageIdSpec)> = opt_in
                .iter()
                // alloc: each `-p`'s spec and its text, to sort and dedup them as cargo does.
                .map(|it| PackageIdSpec::parse(it).map(|spec| (spec.to_string(), spec)))
                // alloc: as above.
                .collect::<Result<_>>()?;
            specs.sort_by(|a, b| a.0.cmp(&b.0));
            specs.dedup_by(|a, b| a.0 == b.0);
            // alloc: the specs selected.
            specs.into_iter().map(|(_, spec)| spec).collect()
        }
        Packages::Default => {
            let named = |name: &str| member_ids.iter().find(|id| id.name().as_str() == name);
            let defaults = ws.default_members().map(|(_, m)| named(&m.package_id.name));
            // alloc: the default members.
            let defaults: Option<Vec<&PackageId>> = defaults.collect();
            defaults
                .context("a default member that is not a member")?
                .iter()
                .map(|id| id.to_spec())
                // alloc: the specs selected.
                .collect()
        }
    })
}

/// Writes `pieces` into `out`, in `style` when the output is colored.
fn paint(out: &mut String, color: bool, style: &str, pieces: &[&str]) {
    if color {
        out.push_str(style);
    }
    for piece in pieces {
        out.push_str(piece);
    }
    if color {
        out.push_str(RESET);
    }
}

struct Printer<'p, 'a> {
    graph: &'p Graph<'a>,
    format: &'p format::Pattern<'p>,
    labels: &'p mut format::Labels,
    symbols: &'static Symbols,
    pruned: &'p Bits,
    members: &'p Bits,
    opts: &'p Options,
    /// A dependency already printed is marked `(*)` (unless --no-dedupe).
    visited: Bits,
    /// The nodes of the lines above the current one, to detect cycles when --no-dedupe is
    /// used.
    on_stack: Bits,
    /// Of each level printed, its line's tag and whether a later sibling continues it.
    levels: Vec<(Tag, bool)>,
}

impl Printer<'_, '_> {
    /// Prints a tree for each given root.
    fn print(&mut self, roots: &[NodeId], out: &mut String) {
        for (i, &root_index) in roots.iter().enumerate() {
            if i != 0 {
                out.push('\n');
            }
            self.levels.clear();
            self.print_node(root_index, out);
        }
    }

    /// Prints a package and all of its dependencies.
    fn print_node(&mut self, node_index: NodeId, out: &mut String) {
        let new = self.opts.no_dedupe || self.visited.insert_growing(node_index.index());
        let (symbols, color) = (self.symbols, self.opts.color);
        match self.opts.prefix {
            Prefix::Depth => {
                let _ = write!(out, "{}", self.levels.len());
            }
            Prefix::Indent => {
                if let Some((&(last_tag, last_continues), rest)) = self.levels.split_last() {
                    for &(tag, continues) in rest {
                        let c = if continues { symbols.down } else { " " };
                        paint(out, color, EDGE_COLORS[tag as usize], &[c]);
                        out.push_str("   ");
                    }
                    let c = if last_continues { symbols.tee } else { symbols.ell };
                    paint(out, color, EDGE_COLORS[last_tag as usize], &[c, symbols.right, symbols.right]);
                    out.push(' ');
                }
            }
            Prefix::None => {}
        }

        let in_cycle = self.on_stack.contains(node_index.index());
        // A node with no dependencies has no (*): nothing was left out.
        let has_deps = self.graph.has_outgoing_edges(node_index);
        self.format.write(self.graph, node_index, self.labels, out);
        if !((new && !in_cycle) || !has_deps) {
            out.push(' ');
            paint(out, color, "\x1b[2m\x1b[33m", &["(*)"]);
        }
        out.push('\n');

        if !new || in_cycle {
            return;
        }
        self.on_stack.insert_growing(node_index.index());
        for tag in 0..TAGS as Tag {
            self.print_dependencies(node_index, tag, out);
        }
        self.on_stack.remove(node_index.index());
    }

    /// Prints all the dependencies of a package for the given dependency kind.
    fn print_dependencies(&mut self, node_index: NodeId, tag: Tag, out: &mut String) {
        let graph = self.graph;
        let deps = graph.edges_of(node_index, tag);
        if deps.is_empty() {
            return;
        }
        let name = match tag {
            BUILD => Some(("\x1b[94m\x1b[1m", "[build-dependencies]")),
            DEV => Some(("\x1b[96m\x1b[1m", "[dev-dependencies]")),
            _ => None,
        };
        let color = self.opts.color;
        if let (Prefix::Indent, Some((style, name))) = (self.opts.prefix, name) {
            for &(tag, continues) in &self.levels {
                let c = if continues { self.symbols.down } else { " " };
                paint(out, color, EDGE_COLORS[tag as usize], &[c]);
                out.push_str("   ");
            }
            paint(out, color, style, &[name]);
            out.push('\n');
        }

        let (max_display_depth, filter_non_workspace_member) = match self.opts.display_depth {
            DisplayDepth::MaxDisplayDepth(max) => (max, false),
            DisplayDepth::Workspace => (u32::MAX, true),
        };
        // Current level exceeds maximum display depth. Skip.
        if self.levels.len() + 1 > max_display_depth as usize {
            return;
        }

        let (members, pruned) = (self.members, self.pruned);
        let keep = |dep: &Edge| match graph.node(dep.node()) {
            Node::Package { pkg, .. } => {
                !(filter_non_workspace_member && !members.contains(pkg.index())) && !pruned.contains(pkg.index())
            }
            Node::Feature { .. } => true,
        };
        let mut next = deps.iter().position(keep);
        while let Some(i) = next {
            next = deps[i + 1..].iter().position(keep).map(|j| i + 1 + j);
            self.levels.push((tag, next.is_some()));
            self.print_node(deps[i].node(), out);
            self.levels.pop();
        }
    }
}
