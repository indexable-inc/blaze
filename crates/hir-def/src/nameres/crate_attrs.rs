//! A crate's inner attributes as the type checker asks them (`#![feature]`,
//! `#![recursion_limit]`, `#![blaze::*]` and the rest), read from its root file's item tree.
//! Only the crate whose text the task holds is read so: the `CrateRoot` record holds
//! `no_std`, `no_core` and the prelude, none of these, so a question of another crate's stops
//! with a typed `NotBuilt` naming its root module (reconnect.md, "What the new-layer crates
//! lack": the crate attributes in `CrateRoot`).

use std::alloc::Global;

use base_db::{Crate, Db};
use intern::{Symbol, sym};

use crate::{
    item_tree::{AttrsOrCfgRef, file_item_tree},
    unstable_features::UnstableFeatures,
};

/// What a crate's inner attributes say.
#[derive(Clone, Default)]
pub struct CrateAttrs {
    /// Custom tool modules registered with `#![register_tool]`.
    pub registered_tools: Vec<Symbol, Global>,
    /// Unstable features enabled with `#![feature(A, B)]`, and the ones a blaze attribute or
    /// edition enters as if written.
    pub unstable_features: UnstableFeatures,
    /// `#![rustc_coherence_is_core]`.
    pub rustc_coherence_is_core: bool,
    /// A crate of procedural macros: its target's kind, or `#![crate_type = "proc-macro"]`.
    pub proc_macro: bool,
    /// `#![blaze::arbitrary_ints]` (docs/v2/plan/design/arbitrary-ints.md).
    pub arbitrary_ints: bool,
    /// `#![blaze::handles]` (docs/v2/plan/design/edition.md, change 1).
    pub handles: bool,
    /// `#![blaze::views]` (docs/v2/plan/design/edition.md, change 3).
    pub views: bool,
    /// `#![blaze::pattern_types]` (docs/v2/plan/design/edition.md, section 8).
    pub pattern_types: bool,
    /// `#![blaze::early_drop]` (docs/v2/plan/design/early-drop.md).
    pub early_drop: bool,
    /// `#![recursion_limit = "N"]`.
    pub recursion_limit: Option<u32>,
}

impl PartialEq for CrateAttrs {
    fn eq(&self, other: &Self) -> bool {
        self.registered_tools == other.registered_tools
            && self.unstable_features.iter().eq(other.unstable_features.iter())
            && self.rustc_coherence_is_core == other.rustc_coherence_is_core
            && self.proc_macro == other.proc_macro
            && self.arbitrary_ints == other.arbitrary_ints
            && self.handles == other.handles
            && self.views == other.views
            && self.pattern_types == other.pattern_types
            && self.early_drop == other.early_drop
            && self.recursion_limit == other.recursion_limit
    }
}

impl Eq for CrateAttrs {}

impl std::fmt::Debug for CrateAttrs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrateAttrs")
            .field("proc_macro", &self.proc_macro)
            .field("recursion_limit", &self.recursion_limit)
            .finish_non_exhaustive()
    }
}

/// Whether `path` is `blaze::<name>`.
fn is_blaze(path: &hir_expand::mod_path::ModPath, name: &str) -> bool {
    matches!(path.segments(), [tool, it] if tool.symbol().as_str() == "blaze" && it.symbol().as_str() == name)
}

/// The inner attributes of `krate`'s root file.
#[blaze_memo::tracked(returns(ref), owns, mem_tag = Nameres)]
pub fn crate_attrs(db: &Db, krate: Crate) -> CrateAttrs {
    crate::nameres::demand_text(db, krate, "the crate attributes of a crate without text");
    let krate_data = krate.data(db);
    let root: hir_expand::HirFileId = krate.root_file_id(db).into();
    let item_tree = file_item_tree(db, root, krate);
    let mut out = CrateAttrs::default();
    // A crate of a blaze edition makes every collection in an allocator it was handed, so the
    // allocator API needs no `#![feature]` in it: entered as if written.
    if krate.blaze_edition(db).is_some() {
        for feature in ["allocator_api", "allocator_ext"] {
            out.unstable_features.enable(Symbol::intern(feature));
        }
    }
    out.proc_macro = krate_data.target_kind.is_proc_macro();
    let attrs = match item_tree.top_level_attrs() {
        Some(AttrsOrCfgRef::Enabled { attrs } | AttrsOrCfgRef::CfgDisabled(_, attrs)) => attrs,
        None => return out,
    };
    for attr in attrs.iter().map(|(_, attr)| attr) {
        let path = &attr.path;
        if is_blaze(path, "arbitrary_ints") {
            out.arbitrary_ints = true;
        } else if is_blaze(path, "handles") {
            out.handles = true;
        } else if is_blaze(path, "views") {
            out.views = true;
        } else if is_blaze(path, "pattern_types") {
            out.pattern_types = true;
        } else if is_blaze(path, "early_drop") {
            out.early_drop = true;
        } else if is_blaze(path, "simd") {
            for feature in ["portable_simd", "blaze_simd"] {
                out.unstable_features.enable(Symbol::intern(feature));
            }
        } else if let Some(name) = path.as_ident() {
            match () {
                () if *name == sym::recursion_limit => {
                    out.recursion_limit = attr.string_value().and_then(|it| it.parse().ok());
                }
                () if *name == sym::crate_type => {
                    out.proc_macro |= attr.string_value() == Some("proc-macro");
                }
                () if *name == sym::rustc_coherence_is_core => out.rustc_coherence_is_core = true,
                () if *name == sym::feature => {
                    let feats = attr.parse_path_comma_token_tree(db, root.context_arena());
                    for (feat, _, _) in feats.into_iter().flatten() {
                        if let [name] = feat.segments() {
                            // clone: an interned symbol
                            out.unstable_features.enable(name.symbol().clone());
                        }
                    }
                }
                () if *name == sym::register_tool => {
                    if let Some(ident) = attr.single_ident_value() {
                        out.registered_tools.push(ident.sym);
                    }
                }
                () => {}
            }
        }
    }
    out
}
