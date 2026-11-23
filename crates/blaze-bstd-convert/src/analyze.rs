//! Sans-IO workspace and Rust AST analyzer.
//!
//! Takes an in-memory map of `(relative_path -> UTF-8 source text)` and produces a
//! [`WorkspaceAnalysis`] covering:
//! - every workspace crate and its Cargo dependencies classified via [`crate::catalog`],
//! - every `std::` / `alloc::` / third-party crate path, macro, and allocator site (parsed via
//!   blaze's lossless Rust 2024 CST/AST `syntax`),
//! - every ambient IO / effect site per function plus the intra-crate transitive effect closure,
//! - every implicit state-machine struct (structs mixing IO handles with phase/flag fields),
//! - every custom proc-macro crate and `#[proc_macro*]` entry point (for `comptime` / `const`
//!   reflection migration),
//! - every hard wall (`unsafe` FFI, `extern "C"`, `thread_local!`, interior-mutable `static`,
//!   fixed-signature trait impls).

use alloc::{
    borrow::ToOwned,
    collections::{BTreeMap, BTreeSet},
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use blaze_own::Alloc;
use blaze_toml::{Document, Limits};
use syntax::{
    AstNode, Edition, SourceFile, SyntaxKind,
    ast::{self, HasArgList, HasAttrs, HasName, HasVisibility},
};

use crate::catalog::{
    DepDisposition, EffectKind, classify_call_effect, classify_macro_effect, is_c_interop_crate,
    is_cpp_interop_crate, is_io_handle_type, is_std_core_module, lookup_crate,
};

/// Upper bound on call-graph effect propagation iterations (`bounded-progress`).
const MAX_CALLGRAPH_ITERS: usize = 64;

/// A pure in-memory snapshot of a Rust workspace (`relative_path -> text`).
#[derive(Debug, Clone, Default)]
pub struct WorkspaceSnapshot {
    pub files: BTreeMap<String, String>,
}

/// Classification of one Cargo dependency edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepEdge {
    pub package: String,
    pub section: DepSection,
    pub is_workspace_internal: bool,
    pub disposition: Option<DepDisposition>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepSection {
    Normal,
    Dev,
    Build,
}

impl DepSection {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "dependencies",
            Self::Dev => "dev-dependencies",
            Self::Build => "build-dependencies",
        }
    }
}

/// One detected IO/effect site inside a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectSite {
    pub kind: EffectKind,
    pub line: usize,
    pub snippet: String,
}

/// One analyzed Rust function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnAnalysis {
    pub file: String,
    pub name: String,
    pub qualified_name: String,
    pub line: usize,
    pub is_async: bool,
    pub is_pub: bool,
    pub is_unsafe: bool,
    pub in_trait_impl: Option<String>,
    pub direct_effects: Vec<EffectSite>,
    pub transitive_effects: BTreeSet<EffectKind>,
    pub callees: BTreeSet<String>,
    pub has_control_flow_across_io: bool,
    pub alloc_sites: usize,
}

impl FnAnalysis {
    /// Whether this function is a strong candidate to compile into an explicit sans-IO state machine.
    pub fn is_sm_candidate(&self) -> bool {
        if self.in_trait_impl.is_some() || self.is_unsafe {
            return false;
        }
        self.is_async
            || self.direct_effects.len() >= 2
            || (self.direct_effects.len() == 1 && self.has_control_flow_across_io)
    }
}

/// One struct detected as holding an implicit state machine or mixed IO + domain state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructSmCandidate {
    pub file: String,
    pub name: String,
    pub line: usize,
    pub io_fields: Vec<(String, String)>,
    pub phase_fields: Vec<(String, String)>,
    pub bool_flag_fields: Vec<String>,
    pub option_fields: Vec<(String, String)>,
    pub other_fields: Vec<(String, String)>,
}

impl StructSmCandidate {
    pub fn reason(&self) -> &'static str {
        if !self.io_fields.is_empty() && (!self.phase_fields.is_empty() || !self.bool_flag_fields.is_empty()) {
            "mixes live IO handles with phase/flag state (split into sans-IO core state enum + thin bstd driver)"
        } else if !self.io_fields.is_empty() {
            "holds live IO/clock handles inside a domain struct (separate sans-IO core from bstd driver)"
        } else {
            "implicit state machine via multiple bool/Option/phase fields (encode states in one enum)"
        }
    }
}

/// What kind of custom proc macro entry point was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProcMacroKind {
    Derive,
    Attribute,
    FunctionLike,
}

impl ProcMacroKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Derive => "proc_macro_derive",
            Self::Attribute => "proc_macro_attribute",
            Self::FunctionLike => "proc_macro",
        }
    }
}

/// Detected pattern inside a custom proc macro, used to recommend the best `comptime`/`const` rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProcMacroPattern {
    FieldReflection,
    VariantReflection,
    AttributeCodegen,
    TokenDsl,
}

impl ProcMacroPattern {
    pub const fn comptime_recommendation(self) -> &'static str {
        match self {
            Self::FieldReflection => {
                "rewrite as `const fn` over `core::mem::type_info::Type::of::<T>()` / `bstd::comptime` (eliminates `syn`/`quote`/`proc-macro2`)"
            }
            Self::VariantReflection => {
                "rewrite with `bstd::comptime::TaggedEnum` / `bstd::enum_tags!` or `core::mem::type_info` enum reflection"
            }
            Self::AttributeCodegen => {
                "rewrite as a `const` descriptor + declarative `macro_rules!` or `#[blaze::comptime]` reflection"
            }
            Self::TokenDsl => {
                "rewrite as a `const fn` parser over `&str` / declarative `macro_rules!` (zero Wasm proc-macro overhead)"
            }
        }
    }
}

/// One `#[proc_macro*]` entry point found in a custom proc-macro crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcMacroEntry {
    pub file: String,
    pub fn_name: String,
    pub derive_name: Option<String>,
    pub line: usize,
    pub kind: ProcMacroKind,
    pub pattern: ProcMacroPattern,
}

/// A hard wall that cannot be rewritten blindly without human/architectural intervention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wall {
    pub file: String,
    pub line: usize,
    pub kind: WallKind,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WallKind {
    ExternFfi,
    UnsafeBlock,
    ThreadLocal,
    InteriorMutableStatic,
    TraitSignature,
    ParseError,
}

impl WallKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExternFfi => "extern-ffi",
            Self::UnsafeBlock => "unsafe-block",
            Self::ThreadLocal => "thread-local",
            Self::InteriorMutableStatic => "interior-mutable-static",
            Self::TraitSignature => "trait-signature",
            Self::ParseError => "parse-error",
        }
    }
}

/// Detected C / C++ interop usage (`bindgen`, `cc`, `cmake`, `cxx`, `autocxx`, `pkg-config`,
/// `#[cxx::bridge]`, or `include!(concat!(env!("OUT_DIR"), ...))`) that blaze replaces with
/// `blaze::c::import!` / `blaze::cpp::import!` and `[package.metadata.blaze.c]` (decisions D174/D176).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CInteropInfo {
    /// C/C++ build or bridge dependencies/tools detected (`bindgen`, `cc`, `cmake`, `cxx`, etc.).
    pub tools: BTreeSet<String>,
    /// True if C++ is used (`cxx`, `autocxx`, `.cpp(true)`, `.cc`/`.cpp`/`.hpp` files, `-std=c++*`).
    pub is_cpp: bool,
    /// Explicit C/C++ standard if detected (e.g. `"c++20"`, `"c11"`).
    pub std_flag: Option<String>,
    /// Header paths extracted from `build.rs` (`.header("...")`) or `#[cxx::bridge]` (`include!("...")`).
    pub headers: Vec<String>,
    /// Include directories extracted from `build.rs` (`.include("...")`, `-I...`).
    pub include_dirs: Vec<String>,
    /// C/C++ source files extracted from `build.rs` (`.file("...")`, `.files([...])`).
    pub source_files: Vec<String>,
    /// Preprocessor defines extracted from `build.rs` (`.define("K", ...)` or `-D...`).
    pub defines: Vec<(String, Option<String>)>,
    /// `include!(concat!(env!("OUT_DIR"), ...))` locations in `src/` to replace.
    pub out_dir_includes: Vec<String>,
    /// True if this crate has a `build.rs`.
    pub has_build_rs: bool,
}

impl CInteropInfo {
    pub fn is_present(&self) -> bool {
        !self.tools.is_empty()
    }

    pub fn import_macro(&self) -> &'static str {
        if self.is_cpp { "blaze::cpp::import!" } else { "blaze::c::import!" }
    }
}

/// Category of bare primitive that should become an explicit `bstd::newtype` (rules 76 & 85).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NewtypeKind {
    /// Affine `Offset` + `Len` pair (`Offset + Len -> Offset`, `Offset - Offset -> Len`, `Offset + Offset` forbidden).
    AffineOffsetLen,
    /// Opaque typed id (`*_id: u32/u64/usize` -> `bstd::id::id!` or `bstd::newtype! { opaque ... }`).
    TypedId,
    /// Network port (`port: u16` -> `bstd::newtype::Port`).
    OpaquePort,
    /// Bare time duration (`*_ms` / `*_ns` / `*_us` / `*_secs` -> `bstd::clock::Duration`).
    DurationUnit,
    /// Bare alignment (`align` / `alignment` -> `bstd::newtype::Alignment`, validated power of two).
    AlignmentPow2,
}

impl NewtypeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AffineOffsetLen => "affine-offset-len",
            Self::TypedId => "typed-id",
            Self::OpaquePort => "opaque-port",
            Self::DurationUnit => "duration-unit",
            Self::AlignmentPow2 => "alignment-pow2",
        }
    }
}

/// One primitive-obsession parameter or struct field that should be promoted to a `bstd::newtype`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewtypeCandidate {
    pub file: String,
    pub line: usize,
    pub owner: String,
    pub name: String,
    pub raw_type: String,
    pub kind: NewtypeKind,
    pub suggestion: String,
}

/// Complete analysis for one workspace crate.
#[derive(Debug, Clone)]
pub struct CrateAnalysis {
    pub name: String,
    pub dir: String,
    pub manifest_path: String,
    pub is_proc_macro: bool,
    pub rs_files: Vec<String>,
    pub deps: Vec<DepEdge>,
    pub std_modules: BTreeMap<String, usize>,
    pub core_only_std_sites: usize,
    pub bstd_std_sites: usize,
    pub alloc_sites: usize,
    pub map_sites: usize,
    pub lock_sites: usize,
    pub fmt_macro_sites: usize,
    pub functions: Vec<FnAnalysis>,
    pub struct_sm_candidates: Vec<StructSmCandidate>,
    pub proc_macro_entries: Vec<ProcMacroEntry>,
    pub c_interop: Option<CInteropInfo>,
    pub newtype_candidates: Vec<NewtypeCandidate>,
    pub builtin_derives_used: BTreeMap<String, usize>,
    pub walls: Vec<Wall>,
}

impl CrateAnalysis {
    pub fn external_normal_deps(&self) -> Vec<&DepEdge> {
        self.deps
            .iter()
            .filter(|d| d.section == DepSection::Normal && !d.is_workspace_internal)
            .collect()
    }

    pub fn external_all_deps(&self) -> Vec<&DepEdge> {
        self.deps.iter().filter(|d| !d.is_workspace_internal).collect()
    }

    /// True if every external normal dependency of this crate is `DirectBstd` (can hit 0 external
    /// normal deps automatically!).
    pub fn can_reach_zero_external_deps_direct(&self) -> bool {
        self.external_normal_deps()
            .iter()
            .all(|d| d.disposition == Some(DepDisposition::DirectBstd))
    }

    /// True if every external normal dependency is either `DirectBstd` or `NeedsDecision` (can hit 0
    /// external deps if the user chooses replacement on decision crates).
    pub fn can_reach_zero_external_deps_with_decisions(&self) -> bool {
        self.external_normal_deps().iter().all(|d| {
            matches!(
                d.disposition,
                Some(DepDisposition::DirectBstd | DepDisposition::NeedsDecision)
            )
        })
    }
}

/// Whole-workspace analysis result.
#[derive(Debug, Clone)]
pub struct WorkspaceAnalysis {
    pub crates: Vec<CrateAnalysis>,
    /// External package name -> sorted list of workspace crate names that depend on it.
    pub external_dep_users: BTreeMap<String, BTreeSet<String>>,
    /// Total `.rs` files parsed.
    pub total_rs_files: usize,
    /// Total functions analyzed.
    pub total_functions: usize,
    /// Total direct IO/effect sites found.
    pub total_effect_sites: usize,
    /// Total state-machine function candidates.
    pub total_sm_fn_candidates: usize,
    /// Total implicit state-machine struct candidates.
    pub total_sm_struct_candidates: usize,
    /// Total custom proc-macro crates.
    pub total_proc_macro_crates: usize,
    /// Total crates using C/C++ build scripts or binding generators (`bindgen`/`cc`/`cmake`/`cxx`/`pkg-config`).
    pub total_c_interop_crates: usize,
    /// Total primitive-obsession sites recommended for `bstd::newtype!` / `bstd::id::id!`.
    pub total_newtype_candidates: usize,
}

/// Analyze an entire workspace snapshot without performing any IO.
pub fn analyze_workspace(snapshot: &WorkspaceSnapshot) -> WorkspaceAnalysis {
    // 1. Discover every `Cargo.toml` with a `[package]` table (workspace crates).
    let mut crate_manifests: Vec<(String, String, String, bool, Vec<(String, DepSection, bool)>)> = Vec::new();
    let mut workspace_crate_names: BTreeSet<String> = BTreeSet::new();

    for (path, text) in &snapshot.files {
        if !path.ends_with("Cargo.toml") {
            continue;
        }
        if let Some(parsed) = parse_crate_manifest(path, text) {
            workspace_crate_names.insert(parsed.name.clone());
            let dir = parent_dir(path);
            crate_manifests.push((parsed.name, dir, path.clone(), parsed.is_proc_macro, parsed.deps));
        }
    }

    // Sort longest directory prefix first so nested crates claim their own `.rs` files.
    let mut dirs_for_match: Vec<(&str, usize)> = crate_manifests
        .iter()
        .enumerate()
        .map(|(idx, (_, dir, _, _, _))| (dir.as_str(), idx))
        .collect();
    dirs_for_match.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(b.0)));

    let mut crate_rs_files: Vec<Vec<String>> = vec![Vec::new(); crate_manifests.len()];
    for path in snapshot.files.keys() {
        if !path.ends_with(".rs") {
            continue;
        }
        for (dir, idx) in &dirs_for_match {
            if dir.is_empty() || path.starts_with(&format!("{dir}/")) {
                crate_rs_files[*idx].push(path.clone());
                break;
            }
        }
    }

    // 2. Analyze each crate.
    let mut crates = Vec::with_capacity(crate_manifests.len());
    let mut external_dep_users: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut total_rs_files = 0usize;
    let mut total_functions = 0usize;
    let mut total_effect_sites = 0usize;
    let mut total_sm_fn_candidates = 0usize;
    let mut total_sm_struct_candidates = 0usize;
    let mut total_proc_macro_crates = 0usize;
    let mut total_c_interop_crates = 0usize;
    let mut total_newtype_candidates = 0usize;

    for (idx, (name, dir, manifest_path, is_proc_macro, raw_deps)) in crate_manifests.into_iter().enumerate() {
        let mut deps = Vec::with_capacity(raw_deps.len());
        let mut c_interop = CInteropInfo::default();
        for (pkg, section, has_path_or_ws) in raw_deps {
            if is_c_interop_crate(&pkg) {
                c_interop.tools.insert(pkg.clone());
                if is_cpp_interop_crate(&pkg) {
                    c_interop.is_cpp = true;
                }
            }
            let is_internal = has_path_or_ws || workspace_crate_names.contains(&pkg);
            let (disposition, target) = if is_internal {
                (None, None)
            } else {
                external_dep_users.entry(pkg.clone()).or_default().insert(name.clone());
                match lookup_crate(&pkg) {
                    Some(m) => (Some(m.disposition), Some(m.target.to_owned())),
                    None => (Some(DepDisposition::Unknown), None),
                }
            };
            deps.push(DepEdge { package: pkg, section, is_workspace_internal: is_internal, disposition, target });
        }

        let rs_files = core::mem::take(&mut crate_rs_files[idx]);
        total_rs_files += rs_files.len();
        if is_proc_macro {
            total_proc_macro_crates += 1;
        }

        let mut std_modules: BTreeMap<String, usize> = BTreeMap::new();
        let mut core_only_std_sites = 0usize;
        let mut bstd_std_sites = 0usize;
        let mut alloc_sites = 0usize;
        let mut map_sites = 0usize;
        let mut lock_sites = 0usize;
        let mut fmt_macro_sites = 0usize;
        let mut functions = Vec::new();
        let mut struct_sm_candidates = Vec::new();
        let mut proc_macro_entries = Vec::new();
        let mut newtype_candidates = Vec::new();
        let mut builtin_derives_used: BTreeMap<String, usize> = BTreeMap::new();
        let mut walls = Vec::new();

        for rs_path in &rs_files {
            if let Some(src) = snapshot.files.get(rs_path) {
                if rs_path == "build.rs" || rs_path.ends_with("/build.rs") {
                    c_interop.has_build_rs = true;
                    analyze_build_rs_c_interop(src, &mut c_interop);
                }
                analyze_c_interop_src_markers(rs_path, src, &mut c_interop);
                if !rs_path.ends_with("/build.rs") && rs_path != "build.rs" {
                    analyze_newtype_candidates_in_file(rs_path, src, &mut |c| {
                        newtype_candidates.push(c);
                    });
                }
                analyze_rs_file(
                    rs_path,
                    src,
                    &mut std_modules,
                    &mut core_only_std_sites,
                    &mut bstd_std_sites,
                    &mut alloc_sites,
                    &mut map_sites,
                    &mut lock_sites,
                    &mut fmt_macro_sites,
                    &mut functions,
                    &mut struct_sm_candidates,
                    &mut proc_macro_entries,
                    &mut builtin_derives_used,
                    &mut walls,
                );
            }
        }

        let c_interop = if c_interop.is_present() {
            total_c_interop_crates += 1;
            Some(c_interop)
        } else {
            None
        };

        // Propagate effects transitively across the crate's intra-crate call graph.
        propagate_transitive_effects(&mut functions);

        total_functions += functions.len();
        for f in &functions {
            total_effect_sites += f.direct_effects.len();
            if f.is_sm_candidate() {
                total_sm_fn_candidates += 1;
            }
        }
        total_sm_struct_candidates += struct_sm_candidates.len();
        total_newtype_candidates += newtype_candidates.len();

        crates.push(CrateAnalysis {
            name,
            dir,
            manifest_path,
            is_proc_macro,
            rs_files,
            deps,
            std_modules,
            core_only_std_sites,
            bstd_std_sites,
            alloc_sites,
            map_sites,
            lock_sites,
            fmt_macro_sites,
            functions,
            struct_sm_candidates,
            proc_macro_entries,
            c_interop,
            newtype_candidates,
            builtin_derives_used,
            walls,
        });
    }

    crates.sort_by(|a, b| a.name.cmp(&b.name));

    WorkspaceAnalysis {
        crates,
        external_dep_users,
        total_rs_files,
        total_functions,
        total_effect_sites,
        total_sm_fn_candidates,
        total_sm_struct_candidates,
        total_proc_macro_crates,
        total_c_interop_crates,
        total_newtype_candidates,
    }
}

struct ParsedManifest {
    name: String,
    is_proc_macro: bool,
    deps: Vec<(String, DepSection, bool)>,
}

fn parse_crate_manifest(_path: &str, text: &str) -> Option<ParsedManifest> {
    let alloc = Alloc::global();
    let doc = Document::parse_in(text, Limits::CARGO, alloc).ok()?;
    let root = doc.root();
    let pkg = root.get("package")?;
    let name = pkg.get("name")?.as_str()?.to_owned();

    let is_proc_macro = root
        .get("lib")
        .and_then(|lib| lib.get("proc-macro"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut deps = Vec::new();
    for (sec_key, section) in [
        ("dependencies", DepSection::Normal),
        ("dev-dependencies", DepSection::Dev),
        ("build-dependencies", DepSection::Build),
    ] {
        if let Some(table) = root.get(sec_key) {
            for val in table.children() {
                let k = val.key();
                let pkg_name = val
                    .get("package")
                    .and_then(|p| p.as_str())
                    .unwrap_or(k)
                    .to_owned();
                let is_local = val.get("path").is_some()
                    || val.get("workspace").and_then(|w| w.as_bool()).unwrap_or(false);
                deps.push((pkg_name, section, is_local));
            }
        }
    }

    Some(ParsedManifest { name, is_proc_macro, deps })
}

fn parent_dir(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir.to_owned(),
        None => String::new(),
    }
}

/// Compute 1-based line number from byte offset.
pub fn line_of_offset(src: &str, offset: usize) -> usize {
    let clamped = offset.min(src.len());
    src[..clamped].bytes().filter(|&b| b == b'\n').count() + 1
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal single-pass file accumulator keeping all per-crate counters together"
)]
fn analyze_rs_file(
    file_path: &str,
    src: &str,
    std_modules: &mut BTreeMap<String, usize>,
    core_only_std_sites: &mut usize,
    bstd_std_sites: &mut usize,
    alloc_sites: &mut usize,
    map_sites: &mut usize,
    lock_sites: &mut usize,
    fmt_macro_sites: &mut usize,
    functions: &mut Vec<FnAnalysis>,
    struct_sm_candidates: &mut Vec<StructSmCandidate>,
    proc_macro_entries: &mut Vec<ProcMacroEntry>,
    builtin_derives_used: &mut BTreeMap<String, usize>,
    walls: &mut Vec<Wall>,
) {
    let parse = SourceFile::parse(src, Edition::CURRENT);
    for err in parse.errors() {
        let offset: usize = err.range().start().into();
        walls.push(Wall {
            file: file_path.to_owned(),
            line: line_of_offset(src, offset),
            kind: WallKind::ParseError,
            detail: err.to_string(),
        });
    }
    let root = parse.tree();
    let root_node = root.syntax();

    // Scan all nodes once for paths, macros, structs, functions, statics, extern blocks, derives.
    for node in root_node.descendants() {
        match node.kind() {
            SyntaxKind::PATH => {
                // Only inspect outermost paths (skip sub-qualifier paths inside a longer path).
                if node.parent().is_some_and(|p| p.kind() == SyntaxKind::PATH) {
                    continue;
                }
                let txt = node.text().to_string();
                let compact: String = txt.chars().filter(|c| !c.is_whitespace()).collect();
                if let Some(rest) = compact.strip_prefix("std::").or_else(|| compact.strip_prefix("::std::")) {
                    let mod_name = rest.split("::").next().unwrap_or("");
                    if !mod_name.is_empty() {
                        *std_modules.entry(mod_name.to_owned()).or_default() += 1;
                        if is_std_core_module(mod_name) && !rest.contains("OsStr") && !rest.contains("CString") {
                            *core_only_std_sites += 1;
                        } else {
                            *bstd_std_sites += 1;
                        }
                    }
                } else if compact.starts_with("alloc::") {
                    *bstd_std_sites += 1;
                }

                // Count prelude allocation / collection / lock sites.
                let head = compact.split('<').next().unwrap_or(&compact);
                match head {
                    "Vec" | "Vec::new" | "Vec::with_capacity" | "String" | "String::new" | "String::from"
                    | "Box" | "Box::new" | "Arc" | "Arc::new" | "Rc" | "Rc::new" => {
                        *alloc_sites += 1;
                    }
                    "HashMap" | "HashSet" | "BTreeMap" | "BTreeSet" | "FxHashMap" | "FxHashSet"
                    | "IndexMap" | "IndexSet" => {
                        *map_sites += 1;
                    }
                    "Mutex" | "RwLock" | "Condvar" | "OnceLock" | "LazyLock" => {
                        *lock_sites += 1;
                    }
                    _ => {}
                }
            }
            SyntaxKind::MACRO_CALL => {
                if let Some(mc) = ast::MacroCall::cast(node) {
                    let mname = mc.path().map(|p| p.syntax().text().to_string()).unwrap_or_default();
                    let leaf = mname.rsplit("::").next().unwrap_or(&mname).trim();
                    match leaf {
                        "format" | "vec" => {
                            *alloc_sites += 1;
                            if leaf == "format" {
                                *fmt_macro_sites += 1;
                            }
                        }
                        "println" | "eprintln" | "print" | "eprint" | "write" | "writeln" => {
                            *fmt_macro_sites += 1;
                        }
                        "thread_local" => {
                            let off: usize = mc.syntax().text_range().start().into();
                            walls.push(Wall {
                                file: file_path.to_owned(),
                                line: line_of_offset(src, off),
                                kind: WallKind::ThreadLocal,
                                detail: "thread_local! static (replace with bstd::workers::Slot or explicit context)".to_owned(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            SyntaxKind::ATTR => {
                if let Some(attr) = ast::Attr::cast(node) {
                    let txt = attr.syntax().text().to_string();
                    if txt.contains("derive(") {
                        for derive_name in [
                            "Serialize",
                            "Deserialize",
                            "Error",
                            "Pod",
                            "Zeroable",
                            "JsonSchema",
                            "Archive",
                            "Component",
                        ] {
                            if txt.contains(derive_name) {
                                *builtin_derives_used.entry(derive_name.to_owned()).or_default() += 1;
                            }
                        }
                    }
                }
            }
            SyntaxKind::EXTERN_BLOCK => {
                let off: usize = node.text_range().start().into();
                walls.push(Wall {
                    file: file_path.to_owned(),
                    line: line_of_offset(src, off),
                    kind: WallKind::ExternFfi,
                    detail: "extern FFI block (keep isolated in a sys/driver crate)".to_owned(),
                });
            }
            SyntaxKind::STATIC => {
                let txt = node.text().to_string();
                if txt.contains("Mutex")
                    || txt.contains("RwLock")
                    || txt.contains("OnceLock")
                    || txt.contains("LazyLock")
                    || txt.contains("static mut")
                {
                    let off: usize = node.text_range().start().into();
                    walls.push(Wall {
                        file: file_path.to_owned(),
                        line: line_of_offset(src, off),
                        kind: WallKind::InteriorMutableStatic,
                        detail: "global interior-mutable static (inject explicit state through Cx/driver)".to_owned(),
                    });
                }
            }
            SyntaxKind::STRUCT => {
                if let Some(st) = ast::Struct::cast(node) {
                    if let Some(cand) = inspect_struct_for_sm(file_path, src, &st) {
                        struct_sm_candidates.push(cand);
                    }
                }
            }
            SyntaxKind::FN => {
                if let Some(func) = ast::Fn::cast(node) {
                    // Check if this function is a custom proc-macro entry point.
                    if let Some(pm) = inspect_fn_for_proc_macro(file_path, src, &func) {
                        proc_macro_entries.push(pm);
                    }
                    // Analyze function body for effects, calls, and control flow.
                    if let Some(fa) = analyze_fn(file_path, src, &func) {
                        functions.push(fa);
                    }
                }
            }
            _ => {}
        }
    }
}

fn inspect_struct_for_sm(file_path: &str, src: &str, st: &ast::Struct) -> Option<StructSmCandidate> {
    let name = st.name()?.text().to_string();
    let ast::FieldList::RecordFieldList(fields) = st.field_list()? else {
        return None;
    };

    let mut io_fields = Vec::new();
    let mut phase_fields = Vec::new();
    let mut bool_flag_fields = Vec::new();
    let mut option_fields = Vec::new();
    let mut other_fields = Vec::new();

    for field in fields.fields() {
        let fname = field.name().map(|n| n.text().to_string()).unwrap_or_default();
        let fty = field.ty().map(|t| t.syntax().text().to_string()).unwrap_or_default();
        let fty_trimmed = fty.trim().to_owned();

        if is_io_handle_type(&fty_trimmed) {
            io_fields.push((fname, fty_trimmed));
        } else if fname.contains("phase")
            || fname.contains("state")
            || fname.contains("stage")
            || fname.contains("mode")
            || fty_trimmed.contains("Phase")
            || fty_trimmed.contains("State")
        {
            phase_fields.push((fname, fty_trimmed));
        } else if fty_trimmed == "bool" {
            bool_flag_fields.push(fname);
        } else if fty_trimmed.starts_with("Option<") {
            option_fields.push((fname, fty_trimmed));
        } else {
            other_fields.push((fname, fty_trimmed));
        }
    }

    // Trigger when:
    // 1. Struct mixes live IO handles with domain state (at least 1 IO field and >= 2 total fields), OR
    // 2. Struct has an implicit state machine (phase field + >= 2 bool/Option fields, or >= 3 bool/Option flags).
    let has_mixed_io = !io_fields.is_empty() && (phase_fields.len() + bool_flag_fields.len() + option_fields.len() + other_fields.len() >= 1);
    let has_implicit_sm = (!phase_fields.is_empty() && (bool_flag_fields.len() + option_fields.len() >= 2))
        || (bool_flag_fields.len() >= 3 && !option_fields.is_empty());

    if !has_mixed_io && !has_implicit_sm {
        return None;
    }

    let off: usize = st.syntax().text_range().start().into();
    Some(StructSmCandidate {
        file: file_path.to_owned(),
        name,
        line: line_of_offset(src, off),
        io_fields,
        phase_fields,
        bool_flag_fields,
        option_fields,
        other_fields,
    })
}

fn inspect_fn_for_proc_macro(file_path: &str, src: &str, func: &ast::Fn) -> Option<ProcMacroEntry> {
    let mut kind = None;
    let mut derive_name = None;

    for attr in func.attrs() {
        let txt = attr.syntax().text().to_string();
        if txt.contains("proc_macro_derive") {
            kind = Some(ProcMacroKind::Derive);
            if let Some(after) = txt.split('(').nth(1) {
                let first = after
                    .split(|c: char| c == ',' || c == ')')
                    .next()
                    .unwrap_or("")
                    .trim();
                if !first.is_empty() {
                    derive_name = Some(first.to_owned());
                }
            }
        } else if txt.contains("proc_macro_attribute") {
            kind = Some(ProcMacroKind::Attribute);
        } else if txt.contains("proc_macro") {
            kind = Some(ProcMacroKind::FunctionLike);
        }
    }

    let kind = kind?;
    let fn_name = func.name()?.text().to_string();
    let body_txt = func.body().map(|b| b.syntax().text().to_string()).unwrap_or_default();

    let pattern = if body_txt.contains("ItemEnum") || body_txt.contains("variants") {
        ProcMacroPattern::VariantReflection
    } else if body_txt.contains("ItemStruct") || body_txt.contains("Fields") || body_txt.contains("offset_of") {
        ProcMacroPattern::FieldReflection
    } else if kind == ProcMacroKind::Attribute {
        ProcMacroPattern::AttributeCodegen
    } else {
        ProcMacroPattern::TokenDsl
    };

    let off: usize = func.syntax().text_range().start().into();
    Some(ProcMacroEntry {
        file: file_path.to_owned(),
        fn_name,
        derive_name,
        line: line_of_offset(src, off),
        kind,
        pattern,
    })
}

fn analyze_fn(file_path: &str, src: &str, func: &ast::Fn) -> Option<FnAnalysis> {
    let body = func.body()?;
    let name = func.name()?.text().to_string();
    let off: usize = func.syntax().text_range().start().into();
    let is_async = func.async_token().is_some();
    let is_pub = func.visibility().is_some();
    let is_unsafe = func.unsafe_token().is_some();

    // Detect whether this fn lives inside an `impl Trait for Type` or inherent `impl Type`.
    let mut in_trait_impl = None;
    let mut impl_prefix = None;
    for anc in func.syntax().ancestors().skip(1) {
        if let Some(imp) = ast::Impl::cast(anc) {
            if let Some(tr) = imp.trait_() {
                in_trait_impl = Some(tr.syntax().text().to_string());
            }
            if let Some(self_ty) = imp.self_ty() {
                impl_prefix = Some(self_ty.syntax().text().to_string());
            }
            break;
        }
    }

    let qualified_name = match impl_prefix {
        Some(ty) => format!("{ty}::{name}"),
        None => name.clone(),
    };

    let mut direct_effects = Vec::new();
    let mut callees = BTreeSet::new();
    let mut alloc_sites = 0usize;
    let mut has_control_flow = false;

    for node in body.syntax().descendants() {
        match node.kind() {
            SyntaxKind::IF_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::LOOP_EXPR | SyntaxKind::FOR_EXPR | SyntaxKind::MATCH_EXPR => {
                has_control_flow = true;
            }
            SyntaxKind::CALL_EXPR => {
                if let Some(call) = ast::CallExpr::cast(node) {
                    if let Some(callee_expr) = call.expr() {
                        let callee_txt: String = callee_expr
                            .syntax()
                            .text()
                            .to_string()
                            .chars()
                            .filter(|c| !c.is_whitespace())
                            .collect();
                        if let Some(eff) = classify_call_effect(&callee_txt) {
                            let coff: usize = call.syntax().text_range().start().into();
                            direct_effects.push(EffectSite {
                                kind: eff,
                                line: line_of_offset(src, coff),
                                snippet: truncate_snippet(&call.syntax().text().to_string(), 96),
                            });
                        } else {
                            let leaf = callee_txt.rsplit("::").next().unwrap_or(&callee_txt);
                            if !leaf.is_empty() && leaf.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
                                callees.insert(leaf.to_owned());
                            }
                        }
                        if matches!(
                            callee_txt.as_str(),
                            "Vec::new" | "Vec::with_capacity" | "String::new" | "String::from" | "Box::new" | "Arc::new"
                        ) {
                            alloc_sites += 1;
                        }
                    }
                }
            }
            SyntaxKind::METHOD_CALL_EXPR => {
                if let Some(mc) = ast::MethodCallExpr::cast(node) {
                    let mname = mc.name_ref().map(|n| n.text().to_string()).unwrap_or_default();
                    let recv_txt = mc.receiver().map(|r| r.syntax().text().to_string()).unwrap_or_default();
                    if matches!(mname.as_str(), "read_to_end" | "read_exact" | "read_to_string")
                        && (recv_txt.contains("sock") || recv_txt.contains("stream") || recv_txt.contains("conn"))
                    {
                        let coff: usize = mc.syntax().text_range().start().into();
                        direct_effects.push(EffectSite {
                            kind: EffectKind::NetRead,
                            line: line_of_offset(src, coff),
                            snippet: truncate_snippet(&mc.syntax().text().to_string(), 96),
                        });
                    } else if matches!(mname.as_str(), "write" | "write_all")
                        && (recv_txt.contains("sock") || recv_txt.contains("stream") || recv_txt.contains("conn"))
                    {
                        let coff: usize = mc.syntax().text_range().start().into();
                        direct_effects.push(EffectSite {
                            kind: EffectKind::NetWrite,
                            line: line_of_offset(src, coff),
                            snippet: truncate_snippet(&mc.syntax().text().to_string(), 96),
                        });
                    } else if matches!(mname.as_str(), "to_string" | "to_owned" | "to_vec") {
                        alloc_sites += 1;
                    }
                }
            }
            SyntaxKind::MACRO_CALL => {
                if let Some(mc) = ast::MacroCall::cast(node) {
                    let mname = mc.path().map(|p| p.syntax().text().to_string()).unwrap_or_default();
                    let leaf = mname.rsplit("::").next().unwrap_or(&mname).trim();
                    if let Some(eff) = classify_macro_effect(leaf) {
                        let coff: usize = mc.syntax().text_range().start().into();
                        direct_effects.push(EffectSite {
                            kind: eff,
                            line: line_of_offset(src, coff),
                            snippet: truncate_snippet(&mc.syntax().text().to_string(), 96),
                        });
                    } else if matches!(leaf, "format" | "vec") {
                        alloc_sites += 1;
                    }
                }
            }
            SyntaxKind::AWAIT_EXPR => {
                let coff: usize = node.text_range().start().into();
                direct_effects.push(EffectSite {
                    kind: EffectKind::AwaitFuture,
                    line: line_of_offset(src, coff),
                    snippet: truncate_snippet(&node.text().to_string(), 96),
                });
            }
            _ => {}
        }
    }

    let transitive_effects: BTreeSet<EffectKind> = direct_effects.iter().map(|e| e.kind).collect();
    let has_control_flow_across_io = has_control_flow && !direct_effects.is_empty();

    Some(FnAnalysis {
        file: file_path.to_owned(),
        name,
        qualified_name,
        line: line_of_offset(src, off),
        is_async,
        is_pub,
        is_unsafe,
        in_trait_impl,
        direct_effects,
        transitive_effects,
        callees,
        has_control_flow_across_io,
        alloc_sites,
    })
}

fn propagate_transitive_effects(functions: &mut [FnAnalysis]) {
    let mut iter = 0;
    while iter < MAX_CALLGRAPH_ITERS {
        iter += 1;
        let mut by_name: BTreeMap<String, BTreeSet<EffectKind>> = BTreeMap::new();
        for f in functions.iter() {
            by_name
                .entry(f.name.clone())
                .or_default()
                .extend(f.transitive_effects.iter().copied());
        }
        let mut changed = false;
        for f in functions.iter_mut() {
            let before = f.transitive_effects.len();
            for callee in &f.callees {
                if let Some(effs) = by_name.get(callee) {
                    f.transitive_effects.extend(effs.iter().copied());
                }
            }
            if f.transitive_effects.len() > before {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

fn truncate_snippet(s: &str, max_len: usize) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() <= max_len {
        one_line
    } else {
        let mut end = max_len.saturating_sub(3);
        while end > 0 && !one_line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &one_line[..end])
    }
}

fn unquote_str_literal(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
        return Some(t[1..t.len() - 1].to_owned());
    }
    if let Some(rest) = t.strip_prefix("r\"") {
        if let Some(inner) = rest.strip_suffix('"') {
            return Some(inner.to_owned());
        }
    }
    if let Some(rest) = t.strip_prefix("r#\"") {
        if let Some(inner) = rest.strip_suffix("\"#") {
            return Some(inner.to_owned());
        }
    }
    None
}

fn push_unique(vec: &mut Vec<String>, val: String) {
    if !val.is_empty() && !vec.contains(&val) {
        vec.push(val);
    }
}

fn looks_like_cpp_path(p: &str) -> bool {
    p.ends_with(".cc")
        || p.ends_with(".cpp")
        || p.ends_with(".cxx")
        || p.ends_with(".hpp")
        || p.ends_with(".hxx")
        || p.ends_with(".hh")
}

/// Extract concrete headers, include directories, source files, defines, and C/C++ standard from
/// a crate's `build.rs` via AST (`bindgen::Builder`, `cc::Build`, `cmake::Config`, `cxx_build`, `pkg_config`).
fn analyze_build_rs_c_interop(src: &str, info: &mut CInteropInfo) {
    let parse = SourceFile::parse(src, Edition::CURRENT);
    let root = parse.tree();

    for node in root.syntax().descendants() {
        match node.kind() {
            SyntaxKind::PATH => {
                if node.parent().is_some_and(|p| p.kind() == SyntaxKind::PATH) {
                    continue;
                }
                let txt: String = node.text().to_string().chars().filter(|c| !c.is_whitespace()).collect();
                for (prefix, tool, is_cpp) in [
                    ("bindgen::", "bindgen", false),
                    ("cc::", "cc", false),
                    ("cmake::", "cmake", false),
                    ("cxx_build::", "cxx-build", true),
                    ("autocxx_build::", "autocxx-build", true),
                    ("pkg_config::", "pkg-config", false),
                ] {
                    if txt.starts_with(prefix) {
                        info.tools.insert(tool.to_owned());
                        if is_cpp {
                            info.is_cpp = true;
                        }
                    }
                }
            }
            SyntaxKind::METHOD_CALL_EXPR => {
                let Some(mc) = ast::MethodCallExpr::cast(node) else { continue };
                let Some(name_ref) = mc.name_ref() else { continue };
                let mname = name_ref.text().to_string();
                let Some(arg_list) = mc.arg_list() else { continue };
                let args: Vec<String> = arg_list.args().map(|a| a.syntax().text().to_string()).collect();

                match mname.as_str() {
                    "header" => {
                        if let Some(h) = args.first().and_then(|a| unquote_str_literal(a)) {
                            if looks_like_cpp_path(&h) {
                                info.is_cpp = true;
                            }
                            push_unique(&mut info.headers, h);
                        }
                    }
                    "include" => {
                        if let Some(dir) = args.first().and_then(|a| unquote_str_literal(a)) {
                            push_unique(&mut info.include_dirs, dir);
                        }
                    }
                    "file" => {
                        if let Some(f) = args.first().and_then(|a| unquote_str_literal(a)) {
                            if looks_like_cpp_path(&f) {
                                info.is_cpp = true;
                            }
                            push_unique(&mut info.source_files, f);
                        }
                    }
                    "files" => {
                        if let Some(arg) = arg_list.args().next() {
                            for lit in arg.syntax().descendants().filter(|n| n.kind() == SyntaxKind::LITERAL) {
                                if let Some(f) = unquote_str_literal(&lit.text().to_string()) {
                                    if looks_like_cpp_path(&f) {
                                        info.is_cpp = true;
                                    }
                                    push_unique(&mut info.source_files, f);
                                }
                            }
                        }
                    }
                    "define" => {
                        if let Some(k) = args.first().and_then(|a| unquote_str_literal(a)) {
                            let val = args.get(1).and_then(|a| {
                                let t = a.trim();
                                if t == "None" {
                                    None
                                } else if let Some(inner) = t.strip_prefix("Some(").and_then(|s| s.strip_suffix(')')) {
                                    unquote_str_literal(inner)
                                } else {
                                    unquote_str_literal(t)
                                }
                            });
                            if !info.defines.iter().any(|(ek, _)| ek == &k) {
                                info.defines.push((k, val));
                            }
                        }
                    }
                    "cpp" => {
                        if args.first().is_some_and(|a| a.trim() == "true") {
                            info.is_cpp = true;
                        }
                    }
                    "std" => {
                        if let Some(s) = args.first().and_then(|a| unquote_str_literal(a)) {
                            if s.starts_with("c++") || s.starts_with("gnu++") {
                                info.is_cpp = true;
                            }
                            info.std_flag = Some(s);
                        }
                    }
                    "clang_arg" | "flag" | "flag_if_supported" => {
                        if let Some(flag) = args.first().and_then(|a| unquote_str_literal(a)) {
                            if let Some(inc) = flag.strip_prefix("-I") {
                                push_unique(&mut info.include_dirs, inc.to_owned());
                            } else if let Some(def) = flag.strip_prefix("-D") {
                                let (k, v) = match def.split_once('=') {
                                    Some((k, v)) => (k.to_owned(), Some(v.to_owned())),
                                    None => (def.to_owned(), None),
                                };
                                if !info.defines.iter().any(|(ek, _)| ek == &k) {
                                    info.defines.push((k, v));
                                }
                            } else if let Some(std_v) = flag.strip_prefix("-std=") {
                                if std_v.starts_with("c++") || std_v.starts_with("gnu++") {
                                    info.is_cpp = true;
                                }
                                info.std_flag = Some(std_v.to_owned());
                            } else if flag == "-xc++" || flag == "c++" {
                                info.is_cpp = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Detect `#[cxx::bridge]` attributes and `include!(concat!(env!("OUT_DIR"), ...))` bindgen sites in `src/`.
fn analyze_c_interop_src_markers(file_path: &str, src: &str, info: &mut CInteropInfo) {
    let parse = SourceFile::parse(src, Edition::CURRENT);
    let root = parse.tree();

    for node in root.syntax().descendants() {
        match node.kind() {
            SyntaxKind::ATTR => {
                let txt = node.text().to_string();
                if txt.contains("cxx::bridge") {
                    info.tools.insert("cxx".to_owned());
                    info.is_cpp = true;
                }
            }
            SyntaxKind::MACRO_CALL => {
                let Some(mc) = ast::MacroCall::cast(node) else { continue };
                let mpath = mc
                    .path()
                    .map(|p| p.syntax().text().to_string().chars().filter(|c| !c.is_whitespace()).collect::<String>())
                    .unwrap_or_default();
                if mpath == "include" || mpath.ends_with("::include") {
                    let body = mc.token_tree().map(|t| t.syntax().text().to_string()).unwrap_or_default();
                    if body.contains("OUT_DIR") {
                        let line = line_of_offset(src, mc.syntax().text_range().start().into());
                        push_unique(&mut info.out_dir_includes, format!("{file_path}:{line}"));
                    } else if info.tools.contains("cxx") {
                        // Inside `#[cxx::bridge]`, `include!("path/to/header.h")` names a C++ header.
                        let inner = body.trim().trim_start_matches('(').trim_end_matches(')').trim();
                        if let Some(h) = unquote_str_literal(inner) {
                            push_unique(&mut info.headers, h);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn is_bare_int_type(ty: &str) -> bool {
    matches!(ty.trim(), "u16" | "u32" | "u64" | "usize" | "i32" | "i64" | "isize")
}

/// Detect primitive-obsession parameters and struct fields (`offset`+`len` pairs, `*_id`, `port`,
/// `*_ms`/`*_ns`, `align`) that should become explicit `bstd::newtype!` / `bstd::id::id!` types.
fn analyze_newtype_candidates_in_file(
    file_path: &str,
    src: &str,
    emit: &mut impl FnMut(NewtypeCandidate),
) {
    let parse = SourceFile::parse(src, Edition::CURRENT);
    let root = parse.tree();

    for node in root.syntax().descendants() {
        match node.kind() {
            SyntaxKind::FN => {
                let Some(func) = ast::Fn::cast(node) else { continue };
                let Some(fname) = func.name() else { continue };
                let owner = format!("fn {}", fname.text());
                let line = line_of_offset(src, func.syntax().text_range().start().into());
                let Some(plist) = func.param_list() else { continue };
                let mut owned_pairs: Vec<(String, String)> = Vec::new();
                for p in plist.params() {
                    let Some(pat) = p.pat() else { continue };
                    let Some(ty) = p.ty() else { continue };
                    let pname = pat.syntax().text().to_string();
                    let pname = pname.trim().trim_start_matches("mut ").trim().to_owned();
                    let ty_txt = ty.syntax().text().to_string().trim().to_owned();
                    if is_bare_int_type(&ty_txt) {
                        owned_pairs.push((pname, ty_txt));
                    }
                }
                let borrowed: Vec<(&str, &str)> =
                    owned_pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
                classify_primitive_pairs(file_path, line, &owner, &borrowed, emit);
            }
            SyntaxKind::STRUCT => {
                let Some(st) = ast::Struct::cast(node) else { continue };
                let Some(sname) = st.name() else { continue };
                let owner = format!("struct {}", sname.text());
                let line = line_of_offset(src, st.syntax().text_range().start().into());
                let Some(ast::FieldList::RecordFieldList(rfl)) = st.field_list() else { continue };
                let mut owned_pairs: Vec<(String, String)> = Vec::new();
                for f in rfl.fields() {
                    let Some(fname) = f.name() else { continue };
                    let Some(ty) = f.ty() else { continue };
                    let ty_txt = ty.syntax().text().to_string().trim().to_owned();
                    if is_bare_int_type(&ty_txt) {
                        owned_pairs.push((fname.text().to_string(), ty_txt));
                    }
                }
                let borrowed: Vec<(&str, &str)> =
                    owned_pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
                classify_primitive_pairs(file_path, line, &owner, &borrowed, emit);
            }
            _ => {}
        }
    }
}

fn classify_primitive_pairs(
    file_path: &str,
    line: usize,
    owner: &str,
    pairs: &[(&str, &str)],
    emit: &mut impl FnMut(NewtypeCandidate),
) {
    let has_offset = pairs
        .iter()
        .any(|&(n, _)| matches!(n, "offset" | "byte_offset" | "start" | "pos") || n.ends_with("_offset"));
    let has_len = pairs
        .iter()
        .any(|&(n, _)| matches!(n, "len" | "length" | "byte_len" | "size" | "end") || n.ends_with("_len"));
    if has_offset && has_len {
        let raw_ty = pairs
            .iter()
            .find(|&&(n, _)| matches!(n, "offset" | "byte_offset" | "start" | "pos") || n.ends_with("_offset"))
            .map(|&(_, t)| t)
            .unwrap_or("usize");
        let suggestion = if raw_ty == "u32" {
            "bstd::newtype::{TextOffset, TextLen, TextSpan}".to_owned()
        } else {
            "bstd::newtype::{ByteOffset, ByteLen, ByteSpan}".to_owned()
        };
        emit(NewtypeCandidate {
            file: file_path.to_owned(),
            line,
            owner: owner.to_owned(),
            name: "offset/len".to_owned(),
            raw_type: raw_ty.to_owned(),
            kind: NewtypeKind::AffineOffsetLen,
            suggestion,
        });
    }

    for &(name, ty) in pairs {
        if name.ends_with("_id") && name.len() > 3 {
            let mut pascal = String::with_capacity(name.len());
            write_pascal_id(name, &mut pascal);
            let suggestion = if ty == "u32" {
                format!("bstd::id::id! {{ pub struct {pascal}; }}")
            } else {
                format!("bstd::newtype! {{ opaque pub struct {pascal}({ty}); }}")
            };
            emit(NewtypeCandidate {
                file: file_path.to_owned(),
                line,
                owner: owner.to_owned(),
                name: name.to_owned(),
                raw_type: ty.to_owned(),
                kind: NewtypeKind::TypedId,
                suggestion,
            });
        } else if (name == "port" || name.ends_with("_port")) && ty == "u16" {
            emit(NewtypeCandidate {
                file: file_path.to_owned(),
                line,
                owner: owner.to_owned(),
                name: name.to_owned(),
                raw_type: ty.to_owned(),
                kind: NewtypeKind::OpaquePort,
                suggestion: "bstd::newtype::Port".to_owned(),
            });
        } else if name.ends_with("_ms")
            || name.ends_with("_ns")
            || name.ends_with("_us")
            || name.ends_with("_secs")
            || name == "timeout_ms"
        {
            emit(NewtypeCandidate {
                file: file_path.to_owned(),
                line,
                owner: owner.to_owned(),
                name: name.to_owned(),
                raw_type: ty.to_owned(),
                kind: NewtypeKind::DurationUnit,
                suggestion: "core::time::Duration".to_owned(),
            });
        } else if matches!(name, "align" | "alignment") && matches!(ty, "usize" | "u64") {
            emit(NewtypeCandidate {
                file: file_path.to_owned(),
                line,
                owner: owner.to_owned(),
                name: name.to_owned(),
                raw_type: ty.to_owned(),
                kind: NewtypeKind::AlignmentPow2,
                suggestion: "bstd::newtype::Alignment".to_owned(),
            });
        }
    }
}

fn write_pascal_id(snake: &str, out: &mut impl core::fmt::Write) {
    let mut cap = true;
    for c in snake.chars() {
        if c == '_' {
            cap = true;
        } else if cap {
            let _ = out.write_char(c.to_ascii_uppercase());
            cap = false;
        } else {
            let _ = out.write_char(c);
        }
    }
}
