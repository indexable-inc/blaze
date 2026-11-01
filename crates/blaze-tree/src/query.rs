//! `cargo tree` as a query of blaze's database: [`Tree`] resolves the view's workspace through
//! blaze-units' load queries ([`resolve_view`]) and prints it as [`crate::print_tree`] prints
//! blaze-metadata's.

use blaze_format::SchemaHash;
use blaze_hash::HashSet;
use blaze_ids::ContentId;
use blaze_manifest::files::Files;
use blaze_query::{Cancelled, Cx, Database, Query, QueryKind};
use blaze_units::load::{
    ResolveAsk, Resolving, ViewResolved, key_id, load_error, platform_cfg, resolve_view,
};
use blaze_units::query::{Cbor, Fetch, LoadError, Versioned, producer};
use serde::{Deserialize, Serialize};

use crate::packages::Reads;
use crate::{
    DepKind, DisplayDepth, EdgeKind, Input, Kinds, Options, Packages, Platformed, Prefix, Target,
};

/// `cargo tree`'s command line, as cargo reads it: everything of [`Options`] and the
/// platforms. The rest of the invocation (`--manifest-path`, `--config`, `--locked`,
/// `--offline`) is the view's.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct TreeAsk {
    /// `-p`, `--workspace`, `--exclude` and the features. Whether dev-dependencies are
    /// resolved is `edges`'.
    pub resolve: ResolveAsk,
    /// `--target`, as given: `all` alone for every platform, none for the host.
    pub targets: Vec<String>,
    pub edges: Vec<Edge>,
    pub invert: Vec<String>,
    pub prune: Vec<String>,
    pub prefix: TreePrefix,
    pub no_dedupe: bool,
    pub duplicates: bool,
    pub format: String,
    pub depth: TreeDepth,
    pub no_proc_macro: bool,
    pub public: bool,
    pub unicode: bool,
    pub color: bool,
}

/// One of [`Options::edge_kinds`].
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    Normal,
    Build,
    Dev,
    Features,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum TreePrefix {
    None,
    Indent,
    Depth,
}

/// `--depth`: a number (`u32::MAX` when not given) or `workspace`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum TreeDepth {
    Max(u32),
    Workspace,
}

/// What `cargo tree` came to: its output and warnings, what it would read that no mount
/// covers yet, or cargo's error.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Printed {
    Ready { out: String, warnings: Vec<String> },
    Needs(Vec<Fetch>),
    Failed(LoadError),
}

impl Versioned for Printed {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo tree printed 1");
}

impl TreeAsk {
    /// The options of `cargo tree`, made once per query from its key, which it outlives.
    fn options(&self) -> Options {
        let edge_kinds: HashSet<EdgeKind> = self
            .edges
            .iter()
            .map(|it| match it {
                Edge::Normal => EdgeKind::Dep(DepKind::Normal),
                Edge::Build => EdgeKind::Dep(DepKind::Build),
                Edge::Dev => EdgeKind::Dep(DepKind::Development),
                Edge::Features => EdgeKind::Feature,
            })
            // alloc: the edge kinds, at most four.
            .collect();
        let ask = &self.resolve;
        Options {
            packages: match (ask.workspace, ask.exclude.is_empty(), ask.packages.is_empty()) {
                (false, true, true) => Packages::Default,
                // clone: the key's `-p`s, once per query.
                (false, true, false) => Packages::Packages(ask.packages.clone()),
                // clone: as above.
                (true, true, _) => Packages::All(ask.packages.clone()),
                // clone: the key's `--exclude`s.
                (_, false, _) => Packages::OptOut(ask.exclude.clone()),
            },
            // clone: the key's `--target`s.
            target: Target::from_cli(self.targets.clone()),
            graph_features: edge_kinds.contains(&EdgeKind::Feature),
            edge_kinds,
            // clone: the key's `-i`s.
            invert: self.invert.clone(),
            // clone: the key's `--prune`s.
            pkgs_to_prune: self.prune.clone(),
            prefix: match self.prefix {
                TreePrefix::None => Prefix::None,
                TreePrefix::Indent => Prefix::Indent,
                TreePrefix::Depth => Prefix::Depth,
            },
            no_dedupe: self.no_dedupe,
            duplicates: self.duplicates,
            // clone: the key's `--format`.
            format: self.format.clone(),
            display_depth: match self.depth {
                TreeDepth::Max(depth) => DisplayDepth::MaxDisplayDepth(depth),
                TreeDepth::Workspace => DisplayDepth::Workspace,
            },
            no_proc_macro: self.no_proc_macro,
            public: self.public,
            unicode: self.unicode,
            color: self.color,
        }
    }
}

/// `cargo tree` of the view's workspace.
pub struct Tree;

impl Query for Tree {
    const KIND: QueryKind = QueryKind(0x601c);
    const NAME: &'static str = "cargo tree";
    const PRODUCER: ContentId = producer("blaze v2 cargo tree 1");
    type Key = TreeAsk;
    type Value = Cbor<Printed>;

    fn input(key: &TreeAsk) -> ContentId {
        key_id("blaze v2 cargo tree key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, ask: &TreeAsk) -> Result<Cbor<Printed>, Cancelled> {
        let opts = ask.options();
        if opts.graph_features && opts.duplicates {
            // alloc: cargo's error.
            let what = "the `-e features` flag does not support `--duplicates`".to_owned();
            return Ok(Cbor(Printed::Failed(LoadError { what, at: None })));
        }
        // clone: the key's resolve, once per query.
        let resolve = ResolveAsk { dev_units: ask.edges.contains(&Edge::Dev), ..ask.resolve.clone() };
        Ok(Cbor(match resolve_view(cx, &resolve, |view| print_view(view, &opts))? {
            Resolving::Done((out, warnings)) => Printed::Ready { out, warnings },
            Resolving::Needs(fetches) => Printed::Needs(fetches),
            Resolving::Failed(error) => Printed::Failed(error),
        }))
    }
}

fn print_view<F: Files>(view: &ViewResolved<'_, F>, opts: &Options) -> Result<(String, Vec<String>), LoadError> {
    let (input, kinds) = view_input(view, opts)?;
    // alloc: the text the query returns, one buffer sized for a line per package.
    let mut out = String::with_capacity(crate::LINE * view.resolve.len());
    // alloc: the warnings the query returns, the load's first.
    let mut warnings = view.warnings.to_vec();
    // alloc: a warning, rare.
    crate::print_resolved(&input, opts, &kinds, &mut out, &mut |it: &str| warnings.push(it.to_owned()))
        .map_err(|error| load_error(&error, None))?;
    Ok((out, warnings))
}

/// The view's workspace as [`crate::print_resolved`] reads it, and its platforms.
fn view_input<'a, F: Files>(view: &ViewResolved<'a, F>, opts: &Options) -> Result<(Input<'a>, Kinds), LoadError> {
    let targets: &[String] = match &opts.target {
        Target::Specific(triples) => triples,
        Target::Host | Target::All => &[],
    };
    let platformed = |triple: &str, target: Option<&str>| -> Result<Platformed, LoadError> {
        let cfg = platform_cfg(view.config, triple, view.host, target)?;
        // alloc: a platform's triple, once per `cargo tree`.
        Ok(Platformed { triple: triple.to_owned(), cfg })
    };
    let mut kinds = Kinds { host: platformed(view.host, targets.first().map(String::as_str))?, targets: Vec::with_capacity(targets.len()) };
    for it in targets {
        kinds.targets.push(platformed(it, Some(it))?);
    }
    let input = Input {
        resolve: view.resolve,
        members: view.members,
        member_ids: view.member_ids,
        specs: view.specs,
        cli_features: view.cli_features,
        behavior: view.behavior,
        unification: view.unification,
        packages: Reads::Units(view.reads),
    };
    Ok((input, kinds))
}

/// Registers [`Tree`]; the load's queries it reads are blaze-units' (`blaze_units::load::register`).
pub fn register(db: &mut Database) {
    db.register::<Tree>();
}

#[cfg(test)]
mod tests;
